//! Linux sandbox integration built on Bubblewrap (`bwrap`) for filesystem and
//! network confinement.
//!
//! We can use `--bind` and `--ro-bind` (read-only) to bind host filesystem
//! paths to paths in the sandbox. If networking is restricted, we also set
//! `--unshare-net` to disable *all* network access.
//!
//! When restricting network access, we:
//! - set `--unshare-net` - any requests to `example.com` will fail
//!   - requests to `localhost` will succeed, but it will be an isolated localhost
//!     from the host system.
//! - create a unix socket, and mount it in the sandbox with `--bind`
//! - run a bridge process inside the sandbox that:
//!   - listens on `localhost:<port>` and forwards reads/writes to the socket
//!   - then, runs the untrusted command
//! - on the zed side, we listen to the socket and forward reads/writes to the
//!   internal HTTP proxy
//!
//! If networking is fully blocked or fully allowed, we don't bother with the
//! proxy/socket at all (and simply set/unset `--unshare-net`).
//!
//! This design for networking avoids needing seccomp, a fork/exec dance, and
//! eliminates a race condition involving BPF user notifications.

use anyhow::{Context as _, Result, anyhow, bail};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

/// Re-exec marker for the in-sandbox launcher: it runs inside the sandbox before
/// the real command to (a) validate that bwrap bound the writable grants to the
/// inodes we captured (the bind-source TOCTOU backstop) and (b) run the
/// restricted-network HTTP bridge. See `README.md` for the design.
const LAUNCHER_FLAG: &str = "--zed-linux-sandbox-launcher";
/// Re-exec marker for the WSL-side helper. This runs *inside WSL* (a Linux
/// process) and does what `Sandbox::wrap` + the validation-fd sender do
/// in-process on native Linux: capture the writable binds' `O_PATH` fds, stand
/// up the validation socket, assemble the bwrap invocation, and spawn it. The
/// capture must happen WSL-side because a Windows process holds no Linux fds.
/// See `README.md`. Shared with the Windows side via `crate::WSL_SANDBOX_HELPER_FLAG`.
const WSL_HELPER_FLAG: &str = crate::WSL_SANDBOX_HELPER_FLAG;
const WSL_RESOLVE_FLAG: &str = crate::WSL_SANDBOX_RESOLVE_FLAG;
/// Sentinel argv token meaning "this optional field is absent".
const LAUNCHER_NONE: &str = "-";
const PROXY_SOCKET_SANDBOX_PATH_PREFIX: &str = "/tmp/zed-sandbox";
const VALIDATION_SOCKET_SANDBOX_PATH_PREFIX: &str = "/tmp/zed-sandbox-validate";
const SANDBOX_SETUP_FAILED_EXIT_CODE: i32 = 126;
const PUMP_BUFFER_SIZE: usize = 64 * 1024;
/// Upper bound on writable binds validated in a single `SCM_RIGHTS` message,
/// kept comfortably below the kernel's per-message fd limit (`SCM_MAX_FD`, 253).
/// Exceeding it fails closed rather than silently validating a subset.
const MAX_VALIDATED_BINDS: usize = 200;

/// Network-access setting for a sandboxed command.
///
/// Mirrors [`crate::macos_seatbelt::NetworkAccess`] so Linux and macOS expose
/// the same public shape.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NetworkAccess {
    /// All outbound network blocked (own network namespace).
    #[default]
    None,
    /// Outbound HTTP(S) is available through a loopback proxy port inside the
    /// sandbox. The port is bridged to a host-side Unix socket proxy that
    /// enforces the hostname allowlist.
    LocalhostPort(u16),
    /// All outbound network allowed.
    All,
}

/// Per-command relaxations of the default Bubblewrap (Linux) sandbox.
///
/// Mirrors [`crate::macos_seatbelt::SandboxPermissions`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SandboxPermissions {
    /// Network access policy for the command.
    pub network: NetworkAccess,
    /// Allow unrestricted filesystem writes.
    pub allow_fs_write: bool,
}

/// The outcome of preparing a Linux sandbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LauncherStatus {
    /// No usable `bwrap` binary was found on `PATH` (or bundled).
    BwrapNotFound,
    /// The only `bwrap` found is setuid-root, which we refuse to execute.
    SetuidRejected,
    /// `bwrap` is present but failed to set up the sandbox with our arguments.
    SandboxProbeFailed,
}

impl LauncherStatus {
    /// A human-readable explanation suitable for diagnostics.
    pub fn describe(self) -> &'static str {
        match self {
            LauncherStatus::BwrapNotFound => "no usable `bwrap` binary was found on PATH",
            LauncherStatus::SetuidRejected => {
                "the only available `bwrap` is setuid-root, which Zed refuses to run"
            }
            LauncherStatus::SandboxProbeFailed => {
                "`bwrap` is present but failed to create a sandbox (unprivileged user \
                 namespaces may be disabled)"
            }
        }
    }
}

/// Where a `bwrap` lookup ended up.
enum BwrapLocation {
    /// A usable, non-setuid `bwrap` binary.
    Found(PathBuf),
    /// `bwrap` exists but every candidate is setuid-root (which we won't run).
    OnlySetuid,
    /// No `bwrap` binary was found at all.
    NotFound,
}

fn locate_bwrap() -> BwrapLocation {
    let mut saw_setuid = false;
    for candidate in candidate_bwrap_paths() {
        if !candidate.is_file() {
            continue;
        }
        if is_setuid_root(&candidate) {
            saw_setuid = true;
            continue;
        }
        return BwrapLocation::Found(candidate);
    }
    if saw_setuid {
        BwrapLocation::OnlySetuid
    } else {
        BwrapLocation::NotFound
    }
}

fn candidate_bwrap_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(system) = system_bwrap_path() {
        candidates.push(system);
    }
    if let Some(bundled) = bundled_bwrap_path() {
        candidates.push(bundled);
    }
    candidates
}

fn system_bwrap_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join("bwrap"))
        .find(|candidate| candidate.is_file())
}

fn bundled_bwrap_path() -> Option<PathBuf> {
    None
}

fn is_setuid_root(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => (metadata.mode() & libc::S_ISUID != 0) && metadata.uid() == 0,
        Err(_) => false,
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "the probe is a short-lived background operation that must block on bwrap"
)]
fn probe_bwrap(bwrap: &Path, bwrap_args: &[String]) -> bool {
    // Capture stderr (rather than discarding it) so a failed probe can report
    // *why* bwrap refused — the difference between "user namespaces disabled",
    // "chdir target missing", "no permission", etc. — instead of a bare
    // `SandboxProbeFailed`.
    let output = Command::new(bwrap)
        .args(bwrap_args)
        .arg("--")
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            log::warn!(
                "[sandbox] bwrap probe failed ({}). command: {} {}\nbwrap stderr: {}",
                output.status,
                bwrap.display(),
                bwrap_args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            false
        }
        Err(error) => {
            log::warn!(
                "[sandbox] bwrap probe could not be spawned: {error}. command: {} {}",
                bwrap.display(),
                bwrap_args.join(" ")
            );
            false
        }
    }
}

/// Build the `bwrap` argument list (everything after the `bwrap` program and
/// before the trailing `-- <command>`) for the given policy.
///
/// `proxy_socket_path` is the host pathname Unix socket used for
/// [`NetworkAccess::LocalhostPort`]. It is bind-mounted to a unique path inside
/// the sandbox where the bridge connects to it.
pub fn build_bwrap_args(
    writable_directories: &[&Path],
    protected_paths: &[&Path],
    permissions: SandboxPermissions,
    cwd: Option<&Path>,
    proxy_socket_path: Option<&Path>,
) -> Vec<String> {
    let proxy_socket_sandbox_path = proxy_socket_path
        .filter(|_| matches!(permissions.network, NetworkAccess::LocalhostPort(_)))
        .map(|_| unique_proxy_socket_sandbox_path());
    build_bwrap_args_with_sandbox_paths(
        writable_directories,
        protected_paths,
        permissions,
        cwd,
        proxy_socket_path,
        proxy_socket_sandbox_path.as_deref(),
        None,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "a flat arg list mirrors the bwrap flags this assembles"
)]
fn build_bwrap_args_with_sandbox_paths(
    writable_directories: &[&Path],
    protected_paths: &[&Path],
    permissions: SandboxPermissions,
    cwd: Option<&Path>,
    proxy_socket_path: Option<&Path>,
    proxy_socket_sandbox_path: Option<&Path>,
    validation_socket_path: Option<&Path>,
    validation_socket_sandbox_path: Option<&Path>,
) -> Vec<String> {
    let mut args = Vec::new();

    let root_bind = if permissions.allow_fs_write {
        "--bind"
    } else {
        "--ro-bind"
    };
    push_bind(&mut args, root_bind, "/", "/");

    args.push("--dev".to_string());
    args.push("/dev".to_string());
    args.push("--proc".to_string());
    args.push("/proc".to_string());

    if !permissions.allow_fs_write {
        args.push("--tmpfs".to_string());
        args.push("/tmp".to_string());

        for directory in writable_directories {
            // Bind each writable directory at its *exact* path, **verbatim** —
            // never re-`canonicalize`d here. The path was already resolved once,
            // at capture time, to the pinned inode's current path (`readlink` of
            // the `O_PATH` fd); re-resolving it now would reopen the
            // time-of-check-to-time-of-use gap a malicious swap exploits. We must
            // also never widen to an existing ancestor, so a path that doesn't
            // exist is skipped rather than falling back to a parent. Whatever
            // inode bwrap actually binds is verified after the mounts by the
            // in-sandbox launcher (see `validate_binds`), which fails closed on a
            // mismatch.
            if !directory.exists() {
                continue;
            }
            let path = directory.to_string_lossy().into_owned();
            push_bind(&mut args, "--bind", &path, &path);
        }
    }

    // Protect requested paths by re-binding them read-only *over* any broader rw
    // bind (order matters: later binds win). Unlike Seatbelt, bwrap can't deny
    // writes while allowing content reads with a policy rule, so a protected path
    // is read-only — its contents stay readable but can't be written. A
    // not-yet-existing path can't be bound, so it's skipped.
    for protected_path in protected_paths {
        if !protected_path.exists() {
            continue;
        }
        let path = protected_path.to_string_lossy().into_owned();
        push_bind(&mut args, "--ro-bind", &path, &path);
    }

    for flag in [
        "--unshare-user",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-pid",
        "--unshare-cgroup-try",
        "--die-with-parent",
    ] {
        args.push(flag.to_string());
    }

    match permissions.network {
        NetworkAccess::None => args.push("--unshare-net".to_string()),
        NetworkAccess::LocalhostPort(_) => {
            args.push("--unshare-net".to_string());
            if let Some((proxy_socket_path, proxy_socket_sandbox_path)) =
                proxy_socket_path.zip(proxy_socket_sandbox_path)
            {
                let source = proxy_socket_path.to_string_lossy().into_owned();
                let destination = proxy_socket_sandbox_path.to_string_lossy().into_owned();
                push_bind(&mut args, "--bind", &source, &destination);
            }
        }
        NetworkAccess::All => {}
    }

    // The validation socket is filesystem-based, so it works regardless of the
    // network policy (an `--unshare-net`'d sandbox can't reach an abstract
    // socket, but a bind-mounted one is fine). Bind it after the `/tmp` tmpfs so
    // it isn't shadowed by the overlay.
    if let Some((source, destination)) = validation_socket_path.zip(validation_socket_sandbox_path)
    {
        let source = source.to_string_lossy().into_owned();
        let destination = destination.to_string_lossy().into_owned();
        push_bind(&mut args, "--bind", &source, &destination);
    }

    if let Some(cwd) = cwd {
        args.push("--chdir".to_string());
        args.push(cwd.to_string_lossy().into_owned());
    }

    args
}

/// A unique destination path for the proxy socket bind mount *inside* the
/// sandbox. Each sandboxed command runs in its own mount namespace, so the path
/// only needs to be unique enough to avoid colliding with anything a previous
/// command in the same namespace lineage may have created; combining the pid
/// with a monotonic counter is sufficient and keeps the path predictable for
/// diagnostics.
fn unique_proxy_socket_sandbox_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        "{PROXY_SOCKET_SANDBOX_PATH_PREFIX}-{}-{counter}.sock",
        std::process::id()
    ))
}

/// In-sandbox bind destination for the validation socket. Each sandboxed command
/// runs in its own mount namespace, so this only needs to avoid colliding with
/// other binds in the same namespace.
fn unique_validation_socket_sandbox_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        "{VALIDATION_SOCKET_SANDBOX_PATH_PREFIX}-{}-{counter}.sock",
        std::process::id()
    ))
}

/// Create a private, owner-only directory for the validation listener socket and
/// return `(directory, socket path)`. The socket is a host path (it lives
/// outside the sandbox's `/tmp` tmpfs) and is bind-mounted in at
/// [`unique_validation_socket_sandbox_path`].
///
/// The socket carries the captured `O_PATH` descriptors over `SCM_RIGHTS`, so
/// connect access must be confined to us. Rather than bind in shared `/tmp` and
/// `chmod` after (which leaves a window where another user could connect before
/// the `chmod` lands), we create a `0700` directory up front: `create` is atomic
/// and fails if the path already exists, and `mode(0o700)` makes it
/// owner-only from creation, so no other user can even traverse to the socket.
fn unique_validation_socket_host_path() -> std::io::Result<(PathBuf, PathBuf)> {
    use std::os::unix::fs::DirBuilderExt as _;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "zed-sandbox-validate-{}-{counter}",
        std::process::id()
    ));
    // Clear a stale directory left by a previous run that reused this pid; this
    // only succeeds for a directory we own, so it can't be used to displace
    // another user's squatted path (in which case `create` below fails closed).
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let socket_path = dir.join("validate.sock");
    Ok((dir, socket_path))
}

/// Host endpoint that hands the in-sandbox validator the captured `O_PATH`
/// descriptors for the writable binds.
///
/// Runs in-process: `spawn` starts a short-lived background **thread** (never a
/// separate process) that listens on an owner-only `AF_UNIX` socket and serves
/// the descriptors to **exactly one** client (the in-sandbox validator) via
/// `SCM_RIGHTS`, then closes and removes the socket. The validator connects
/// once, after bwrap has mounted the binds; `SCM_RIGHTS` keeps the descriptors
/// alive in flight, so once the single send completes there is nothing left to
/// serve. Holding the descriptors until that send keeps the captured inodes
/// pinned (so they can't be recycled) up to the moment they're handed off.
///
/// It is owned by the per-command [`Sandbox`](crate::Sandbox); `Drop` wakes a
/// still-blocked accept and removes the socket and its private directory (a
/// no-op if the thread already tore them down). The socket is bind-mounted into
/// the sandbox at [`Self::sandbox_socket_path`].
pub(crate) struct ValidationFdSender {
    host_socket_dir: PathBuf,
    host_socket_path: PathBuf,
    sandbox_socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
}

impl ValidationFdSender {
    /// Start serving `fds` (one per writable bind, in bind order). The caller
    /// must pass the same order to the launcher so each fd lines up with its
    /// bind-destination path.
    pub(crate) fn spawn(fds: Vec<OwnedFd>) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;

        if fds.len() > MAX_VALIDATED_BINDS {
            return Err(std::io::Error::other(format!(
                "too many writable binds to validate ({} > {MAX_VALIDATED_BINDS})",
                fds.len()
            )));
        }
        let (host_socket_dir, host_socket_path) = unique_validation_socket_host_path()?;
        let listener = UnixListener::bind(&host_socket_path)?;
        // The enclosing directory is already `0700`, so this is defense in depth:
        // connect permission on an `AF_UNIX` socket is governed by write
        // permission on the socket file, so keep it owner-only too.
        std::fs::set_permissions(&host_socket_path, std::fs::Permissions::from_mode(0o600))?;
        let sandbox_socket_path = unique_validation_socket_sandbox_path();
        let shutdown = Arc::new(AtomicBool::new(false));

        // We use std threading APIs here because this code is run from both the
        // Linux zed binary and also the WSL helper, which does not have a GPUI
        // runtime.
        thread::Builder::new()
            .name("zed-sandbox-validation".to_string())
            .spawn({
                let shutdown = shutdown.clone();
                let host_socket_path = host_socket_path.clone();
                move || {
                    // `fds` is held until the single send below, keeping the
                    // pinned inodes alive until they're handed to the validator.
                    let raw_fds: Vec<RawFd> = fds.iter().map(|fd| fd.as_raw_fd()).collect();
                    // Serve exactly one client: the in-sandbox validator connects
                    // once. A second connection (e.g. the wake from `Drop`) is
                    // distinguished by the `shutdown` flag and never served.
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if !shutdown.load(Ordering::SeqCst) {
                                if let Err(error) = send_fds(&stream, &raw_fds) {
                                    log::warn!("[sandbox] failed to send validation fds: {error}");
                                }
                            }
                        }
                        Err(error) => {
                            if !shutdown.load(Ordering::SeqCst) {
                                log::warn!("[sandbox] sandbox validation accept failed: {error}");
                            }
                        }
                    }
                    drop(fds);
                    // Close the listener and remove the socket so it can't be
                    // connected to again. The directory is removed by `Drop`.
                    drop(listener);
                    let _ = std::fs::remove_file(&host_socket_path);
                }
            })?;

        Ok(Self {
            host_socket_dir,
            host_socket_path,
            sandbox_socket_path,
            shutdown,
        })
    }

    pub(crate) fn host_socket_path(&self) -> &Path {
        &self.host_socket_path
    }

    pub(crate) fn sandbox_socket_path(&self) -> &Path {
        &self.sandbox_socket_path
    }
}

impl Drop for ValidationFdSender {
    fn drop(&mut self) {
        // If the validator never connected (e.g. bwrap failed before the
        // launcher ran), the thread is still blocked in `accept`. Flag shutdown
        // and poke the socket so the accept returns and the thread exits without
        // serving the descriptors, then remove the socket and its private
        // directory (a no-op if the thread already removed the socket after its
        // single send).
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.host_socket_path);
        let _ = std::fs::remove_file(&self.host_socket_path);
        let _ = std::fs::remove_dir_all(&self.host_socket_dir);
    }
}

fn push_bind(args: &mut Vec<String>, flag: &str, source: &str, destination: &str) {
    args.push(flag.to_string());
    args.push(source.to_string());
    args.push(destination.to_string());
}

fn resolve_bwrap() -> std::result::Result<PathBuf, LauncherStatus> {
    match locate_bwrap() {
        BwrapLocation::Found(path) => Ok(path),
        BwrapLocation::OnlySetuid => Err(LauncherStatus::SetuidRejected),
        BwrapLocation::NotFound => Err(LauncherStatus::BwrapNotFound),
    }
}

fn prepare_sandbox(
    permissions: SandboxPermissions,
) -> std::result::Result<(PathBuf, Vec<String>), LauncherStatus> {
    let bwrap = match resolve_bwrap() {
        Ok(bwrap) => bwrap,
        Err(status) => {
            log::warn!("[sandbox] cannot create sandbox: {}", status.describe());
            return Err(status);
        }
    };
    // The probe only answers "can a sandbox be created on this host at all", so
    // it runs a bare, representative sandbox: no writable grants, no protected
    // Git dirs, no proxy socket, and no `--chdir` into a command's working
    // directory. None of those affect createability, and binding them would make
    // the probe depend on per-command layout (e.g. a worktree under the `/tmp`
    // tmpfs that `--chdir` then can't reach).
    let bwrap_args = build_bwrap_args(&[], &[], permissions, None, None);
    if !probe_bwrap(&bwrap, &bwrap_args) {
        return Err(LauncherStatus::SandboxProbeFailed);
    }
    Ok((bwrap, bwrap_args))
}

/// Check whether an OS sandbox can be created on this host for this policy.
pub fn check_can_create_sandbox(
    permissions: SandboxPermissions,
) -> std::result::Result<(), LauncherStatus> {
    prepare_sandbox(permissions).map(|_| ())
}

/// The host (Zed-side) socket paths for the in-sandbox bind validator.
#[derive(Clone, Copy)]
pub struct ValidationSocket<'a> {
    /// Host pathname of the listener the validator connects back to.
    pub host_socket_path: &'a Path,
    /// In-sandbox path the host socket is bind-mounted to (where the validator
    /// actually connects).
    pub sandbox_socket_path: &'a Path,
}

/// Build the final command line that runs `program` inside Bubblewrap.
///
/// `bridge_program` should be the current Zed executable; it is re-exec'd inside
/// the sandbox as the launcher whenever bind validation and/or the
/// restricted-network bridge are needed, running before the real command.
///
/// The host `proxy_socket_path` and `validation_socket` are each bind-mounted to
/// a per-invocation path inside the sandbox, and those in-sandbox paths are
/// handed to the launcher.
#[allow(
    clippy::too_many_arguments,
    reason = "assembling a bwrap command line is inherently parameter-heavy"
)]
pub fn wrap_invocation(
    bridge_program: &str,
    permissions: SandboxPermissions,
    writable_dirs: &[&Path],
    protected_paths: &[&Path],
    cwd: Option<&Path>,
    program: &str,
    args: &[String],
    proxy_socket_path: Option<&Path>,
    validation_socket: Option<ValidationSocket<'_>>,
) -> Result<(String, Vec<String>)> {
    if matches!(permissions.network, NetworkAccess::LocalhostPort(_)) && proxy_socket_path.is_none()
    {
        bail!("restricted Linux network access requires a proxy Unix socket path");
    }
    if writable_dirs.len() > MAX_VALIDATED_BINDS {
        bail!(
            "too many writable binds to validate ({} > {MAX_VALIDATED_BINDS})",
            writable_dirs.len()
        );
    }

    // Every writable path must already exist: `bwrap` can't bind a nonexistent
    // source, and the command can't create it either (its parent is read-only
    // inside the sandbox). Callers hand us captured canonical paths whose
    // inodes were pinned at policy-construction time, so a missing path here
    // means the location vanished since capture. Never create it with the
    // agent's ambient permissions — recreating from path text would bind a
    // fresh, unapproved object in place of the pinned one (and historically
    // this materialized bogus paths, e.g. a granted *file* path springing into
    // existence as a directory). Running anyway would give the command silently
    // less access than it believes it has — so fail closed with a clear error.
    if !permissions.allow_fs_write {
        for directory in writable_dirs {
            if !directory.exists() {
                bail!(
                    "failed to provide writable sandbox path {}: it no longer exists",
                    directory.display()
                );
            }
        }
    }

    let bwrap = resolve_bwrap().map_err(|status| anyhow!(status.describe()))?;
    let proxy_socket_sandbox_path = match permissions.network {
        NetworkAccess::LocalhostPort(_) => Some(unique_proxy_socket_sandbox_path()),
        NetworkAccess::None | NetworkAccess::All => None,
    };
    let mut bwrap_args = build_bwrap_args_with_sandbox_paths(
        writable_dirs,
        protected_paths,
        permissions,
        cwd,
        proxy_socket_path,
        proxy_socket_sandbox_path.as_deref(),
        validation_socket.map(|socket| socket.host_socket_path),
        validation_socket.map(|socket| socket.sandbox_socket_path),
    );
    bwrap_args.push("--".to_string());

    let bridge = match permissions.network {
        NetworkAccess::LocalhostPort(port) => {
            let proxy_socket_sandbox_path = proxy_socket_sandbox_path
                .as_ref()
                .context("missing in-sandbox proxy socket path")?;
            Some((proxy_socket_sandbox_path.clone(), port))
        }
        NetworkAccess::None | NetworkAccess::All => None,
    };

    // Always route through the in-sandbox launcher, even when there are no
    // writable binds to validate and no restricted-network bridge: the launcher
    // is where the seccomp filter is installed on the untrusted command (see
    // `exec_command` / `run_bridge`). Absent fields are passed as the `-`
    // sentinel, and `run_launcher` then just installs the filter and `exec`s.
    bwrap_args.push(bridge_program.to_string());
    bwrap_args.push(LAUNCHER_FLAG.to_string());
    // Field 1: validation socket (in-sandbox path) or sentinel.
    bwrap_args.push(match validation_socket {
        Some(socket) => socket.sandbox_socket_path.to_string_lossy().into_owned(),
        None => LAUNCHER_NONE.to_string(),
    });
    // Fields 2-3: bridge socket (in-sandbox path) + port, or sentinels.
    match &bridge {
        Some((socket, port)) => {
            bwrap_args.push(socket.to_string_lossy().into_owned());
            bwrap_args.push(port.to_string());
        }
        None => {
            bwrap_args.push(LAUNCHER_NONE.to_string());
            bwrap_args.push(LAUNCHER_NONE.to_string());
        }
    }
    // Field 4: the writable bind-destination paths to validate (count, then
    // the paths), in the same order the host sends their fds.
    let validation_paths: &[&Path] = if validation_socket.is_some() {
        writable_dirs
    } else {
        &[]
    };
    bwrap_args.push(validation_paths.len().to_string());
    for path in validation_paths {
        bwrap_args.push(path.to_string_lossy().into_owned());
    }
    bwrap_args.push("--".to_string());

    bwrap_args.push(program.to_string());
    bwrap_args.extend(args.iter().cloned());

    let bwrap = bwrap
        .to_str()
        .with_context(|| format!("bwrap path contains invalid UTF-8: {}", bwrap.display()))?;
    Ok((bwrap.to_string(), bwrap_args))
}

/// Handle a possible re-exec of this binary as the in-sandbox launcher (bind
/// validator + network bridge). Does not return if it was invoked as one.
pub fn run_launcher_if_invoked() {
    let Some(invocation) = parse_launcher_args(std::env::args_os()) else {
        return;
    };
    let invocation = match invocation {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("zed: malformed sandbox launcher invocation: {error:#}");
            std::process::exit(127);
        }
    };
    run_launcher(invocation);
}

/// A decoded in-sandbox launcher invocation (the `--zed-linux-sandbox-launcher`
/// re-exec). All fields are produced by the trusted host side and parsed before
/// any untrusted command runs.
struct LauncherInvocation {
    /// In-sandbox path of the validation socket, if bind validation is required.
    validation_socket: Option<PathBuf>,
    /// Writable bind-destination paths to validate, in the order the host sends
    /// their fds. Empty when validation isn't required.
    validation_paths: Vec<PathBuf>,
    /// `(in-sandbox proxy socket path, loopback port)` if the restricted-network
    /// bridge is required.
    bridge: Option<(PathBuf, u16)>,
    program: OsString,
    args: Vec<OsString>,
}

fn parse_launcher_args(
    args: impl IntoIterator<Item = OsString>,
) -> Option<Result<LauncherInvocation>> {
    let mut args = args.into_iter();
    args.next()?;
    if args.next()?.to_str() != Some(LAUNCHER_FLAG) {
        return None;
    }
    Some(decode_launcher_args(args))
}

/// Parse an optional field encoded as either a real value or the `-` sentinel.
fn optional_field(value: OsString) -> Option<OsString> {
    if value == LAUNCHER_NONE {
        None
    } else {
        Some(value)
    }
}

fn decode_launcher_args(mut args: impl Iterator<Item = OsString>) -> Result<LauncherInvocation> {
    let validation_socket =
        optional_field(args.next().context("missing validation socket field")?).map(PathBuf::from);
    let bridge_socket =
        optional_field(args.next().context("missing bridge socket field")?).map(PathBuf::from);
    let bridge_port = optional_field(args.next().context("missing bridge port field")?)
        .map(|value| {
            value
                .to_str()
                .context("bridge port is not valid UTF-8")?
                .parse::<u16>()
                .context("invalid bridge port")
        })
        .transpose()?;
    let bridge = match (bridge_socket, bridge_port) {
        (Some(socket), Some(port)) => Some((socket, port)),
        (None, None) => None,
        _ => bail!("bridge socket and port must be set together"),
    };

    let path_count = args
        .next()
        .context("missing validation path count")?
        .to_str()
        .context("validation path count is not valid UTF-8")?
        .parse::<usize>()
        .context("invalid validation path count")?;
    if path_count > MAX_VALIDATED_BINDS {
        bail!("validation path count {path_count} exceeds {MAX_VALIDATED_BINDS}");
    }
    let mut validation_paths = Vec::with_capacity(path_count);
    for _ in 0..path_count {
        validation_paths.push(PathBuf::from(
            args.next().context("missing validation path")?,
        ));
    }

    let separator = args.next().context("missing launcher argument separator")?;
    if separator != "--" {
        bail!("missing launcher argument separator");
    }
    let program = args.next().context("missing program to run")?;
    let args = args.collect();
    Ok(LauncherInvocation {
        validation_socket,
        validation_paths,
        bridge,
        program,
        args,
    })
}

/// The in-sandbox launcher entry point. Runs after bwrap's mounts and before the
/// real command: it verifies the writable binds weren't redirected, optionally
/// starts the restricted-network bridge, then runs the command. Never returns.
fn run_launcher(invocation: LauncherInvocation) -> ! {
    if let Some(socket) = &invocation.validation_socket {
        if let Err(error) = validate_binds(socket, &invocation.validation_paths) {
            // Fail closed: a redirected (or unverifiable) writable bind means the
            // command must not run at all.
            eprintln!("zed: sandbox bind validation failed: {error:#}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    }

    match invocation.bridge {
        Some((socket_path, port)) => {
            run_bridge(socket_path, port, &invocation.program, &invocation.args)
        }
        // No bridge to keep alive, so `exec` the command directly rather than
        // lingering as a parent process.
        None => exec_command(&invocation.program, &invocation.args),
    }
}

/// Verify each writable bind resolves, inside the sandbox, to the exact inode the
/// host captured. Receives the captured `O_PATH` fds over `socket_path` via
/// `SCM_RIGHTS` (in the same order as `paths`), then compares `fstat(received
/// fd)` against `lstat(mounted path)`. Both stats run in this process inside the
/// sandbox, so the comparison needs no cross-namespace assumption about device
/// numbers. Any mismatch — or any failure to obtain the expected number of fds —
/// is an error (the caller fails closed).
fn validate_binds(socket_path: &Path, paths: &[PathBuf]) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to validation socket {}", socket_path.display()))?;
    let fds = recv_fds(&stream).context("receiving validation descriptors")?;
    if fds.len() != paths.len() {
        bail!(
            "expected {} validation descriptor(s), received {}",
            paths.len(),
            fds.len()
        );
    }
    for (fd, path) in fds.iter().zip(paths) {
        let expected = fd_dev_ino(fd.as_fd())
            .with_context(|| format!("fstat of captured descriptor for {}", path.display()))?;
        let mounted = lstat_dev_ino(path)
            .with_context(|| format!("lstat of mounted bind {}", path.display()))?;
        if expected != mounted {
            bail!(
                "writable bind {} was redirected (captured inode {:?}, mounted inode {:?})",
                path.display(),
                expected,
                mounted
            );
        }
    }
    Ok(())
}

/// Build the seccomp-BPF program installed on the untrusted command before it
/// runs. This is the syscall-level half of preventing session-IPC-socket
/// sandbox escapes: a read-only bind mount does not stop `connect()` to a unix
/// socket, so instead we stop the command from ever *obtaining* a non-IP socket.
///
/// The program (default action: allow):
/// - `socket()` is denied (`EPERM`) unless the family is `AF_INET`/`AF_INET6`/
///   `AF_NETLINK` — so no `AF_UNIX` (session IPC) or `AF_VSOCK` (VM host) sockets.
/// - `socketpair()` is allowed only for `AF_UNIX` (a process-local pair that
///   cannot reach anything outside the sandbox).
/// - `io_uring_*` is denied, so its ring ops can't create/connect sockets
///   without going through the filtered syscalls.
/// - `ptrace`/`process_vm_*` are denied.
///
/// `connect`/`recvmsg`/`sendmsg`/`bind`/`listen`/`accept` stay allowed: with no
/// way to create a forbidden socket (and — by fd hygiene — no forbidden fd
/// inherited), there is nothing dangerous for them to act on, and blocking
/// `connect` would break legitimate loopback/proxy use. Foreign-architecture
/// syscalls are killed by seccompiler's arch check, closing the 32-bit-ABI
/// (`socketcall`) bypass.
///
/// Returns `None` on an architecture seccompiler can't target (we ship on
/// x86_64/aarch64, so that is not a configuration we run in practice).
fn build_command_seccomp_program() -> Result<Option<seccompiler::BpfProgram>> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };
    use std::collections::BTreeMap;

    let Ok(target_arch) = TargetArch::try_from(std::env::consts::ARCH) else {
        return Ok(None);
    };

    // `socket(domain, ...)`: deny unless `domain` (arg0, an `int` — compare the
    // low 32 bits) is an allowed IP/netlink family. The rule matches when the
    // family is none of the allowed ones, and a matched rule takes the deny
    // action; an allowed family matches no rule and falls through to `Allow`.
    let socket_deny = SeccompRule::new(vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_INET as u64,
        )?,
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_INET6 as u64,
        )?,
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_NETLINK as u64,
        )?,
    ])?;
    // `socketpair(domain, ...)`: allow only `AF_UNIX`.
    let socketpair_deny = SeccompRule::new(vec![SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        libc::AF_UNIX as u64,
    )?])?;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules.insert(libc::SYS_socket, vec![socket_deny]);
    rules.insert(libc::SYS_socketpair, vec![socketpair_deny]);
    // Unconditional denials (an empty rule chain always takes the match action).
    for syscall in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ] {
        rules.insert(syscall, Vec::new());
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch,
    )
    .context("building sandbox command seccomp filter")?;
    let program =
        BpfProgram::try_from(filter).context("compiling sandbox command seccomp filter")?;
    Ok(Some(program))
}

/// Install [`build_command_seccomp_program`] on the calling thread, which is
/// about to become (or `exec` into) the untrusted command; the filter survives
/// `exec`. `apply_filter` also sets `PR_SET_NO_NEW_PRIVS`. On an unsupported
/// architecture there is no filter to install — log and proceed rather than
/// break the sandbox on a platform we don't ship.
fn install_command_seccomp_filter() -> Result<()> {
    match build_command_seccomp_program()? {
        Some(program) => seccompiler::apply_filter(&program)
            .context("installing sandbox command seccomp filter")?,
        None => log::warn!(
            "[sandbox] seccomp is unavailable on {}; the unix-socket syscall guard \
             was not installed",
            std::env::consts::ARCH
        ),
    }
    Ok(())
}

/// Replace this process with the sandboxed command. Only returns (after logging)
/// if `exec` itself fails.
fn exec_command(program: &OsStr, args: &[OsString]) -> ! {
    // Lock down socket/io_uring/ptrace syscalls right before handing control to
    // the untrusted command; the filter survives `exec`.
    if let Err(error) = install_command_seccomp_filter() {
        eprintln!("zed: failed to install sandbox seccomp filter: {error:#}");
        std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
    }
    let error = Command::new(program).args(args).exec();
    eprintln!("zed: failed to exec sandboxed command: {error}");
    std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
}

#[allow(
    clippy::disallowed_methods,
    reason = "the bridge is an in-sandbox process that must synchronously spawn and wait for the command"
)]
fn run_bridge(socket_path: PathBuf, port: u16, program: &OsStr, program_args: &[OsString]) -> ! {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("zed: failed to bind sandbox proxy bridge: {error}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    };

    if let Err(error) = thread::Builder::new()
        .name("zed-sandbox-bridge".to_string())
        .stack_size(128 * 1024)
        .spawn(move || run_bridge_listener(listener, socket_path))
    {
        eprintln!("zed: failed to spawn sandbox proxy bridge: {error}");
        std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
    }

    // The command runs under the syscall filter, installed in the child via
    // `pre_exec` — *this* bridge process must NOT be filtered, since it keeps
    // using `AF_UNIX` to reach the host proxy for every request the command
    // makes. Build the program before the fork; the child only applies it.
    let seccomp_program = match build_command_seccomp_program() {
        Ok(program) => program,
        Err(error) => {
            eprintln!("zed: failed to build sandbox seccomp filter: {error:#}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    };
    let mut command = Command::new(program);
    command.args(program_args);
    // SAFETY: the closure runs in the forked child after `fork` and before
    // `exec`. It only calls `seccompiler::apply_filter` (a `prctl` on a program
    // built before the fork) — async-signal-safe and allocation-free.
    unsafe {
        command.pre_exec(move || {
            if let Some(program) = &seccomp_program {
                seccompiler::apply_filter(program)
                    .map_err(|error| std::io::Error::other(format!("seccomp: {error}")))?;
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("zed: failed to spawn sandboxed command: {error}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    };

    match child.wait() {
        Ok(status) => {
            if let Some(code) = status.code() {
                std::process::exit(code);
            }
            let signal = status.signal().unwrap_or(1);
            std::process::exit(128 + signal);
        }
        Err(error) => {
            eprintln!("zed: failed to wait for sandboxed command: {error}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    }
}

/// Send `fds` over `stream` in a single message carrying one byte of payload and
/// the descriptors as `SCM_RIGHTS` ancillary data.
fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> std::io::Result<()> {
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
    let payload = [0u8; 1];
    let iov = [std::io::IoSlice::new(&payload)];
    let control = [ControlMessage::ScmRights(fds)];
    sendmsg::<()>(
        stream.as_raw_fd(),
        &iov,
        &control,
        MsgFlags::MSG_NOSIGNAL,
        None,
    )
    .map_err(std::io::Error::from)?;
    Ok(())
}

/// Receive the descriptors sent via `SCM_RIGHTS` on `stream`. They arrive with
/// `O_CLOEXEC` (via `MSG_CMSG_CLOEXEC`) so they don't leak into the command
/// that's `exec`'d after validation. The ancillary buffer is sized for the
/// protocol maximum ([`MAX_VALIDATED_BINDS`]); a sender exceeding it trips
/// `MSG_CTRUNC` and a mismatched count is rejected by the caller.
fn recv_fds(stream: &UnixStream) -> std::io::Result<Vec<OwnedFd>> {
    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
    let mut payload = [0u8; 1];
    let mut iov = [std::io::IoSliceMut::new(&mut payload)];
    let mut control = nix::cmsg_space!([RawFd; MAX_VALIDATED_BINDS]);
    let message = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut control),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )
    .map_err(std::io::Error::from)?;
    if message.flags.contains(MsgFlags::MSG_CTRUNC) {
        return Err(std::io::Error::other(
            "validation descriptors were truncated in transit",
        ));
    }

    let mut fds = Vec::new();
    for control_message in message.cmsgs().map_err(std::io::Error::from)? {
        if let ControlMessageOwned::ScmRights(raw_fds) = control_message {
            for raw in raw_fds {
                // SAFETY: the kernel installs each `SCM_RIGHTS` descriptor as a
                // freshly-allocated, open fd in *our* table for this message
                // (with `O_CLOEXEC` via `MSG_CMSG_CLOEXEC`), so it's valid and
                // can't alias an fd we already hold. `nix`'s `ScmRights` does not
                // take ownership of received fds (closing them is the caller's
                // responsibility), so wrapping each exactly once here makes us
                // their sole owner. Both hold for any peer, which is why this
                // (and `recv_fds`) is sound as a safe fn rather than needing an
                // `unsafe fn` precondition on the caller.
                fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
            }
        }
    }
    Ok(fds)
}

/// `(device, inode)` of the object an already-open descriptor refers to.
fn fd_dev_ino(fd: BorrowedFd<'_>) -> std::io::Result<(u64, u64)> {
    let stat = nix::sys::stat::fstat(fd).map_err(std::io::Error::from)?;
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

/// `(device, inode)` of `path` without following a final symlink.
fn lstat_dev_ino(path: &Path) -> std::io::Result<(u64, u64)> {
    // `symlink_metadata` is `lstat(2)` (it does not follow a final symlink).
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// A decoded WSL-helper invocation (`--wsl-sandbox-helper`). All fields are
/// produced by the trusted Windows side and parsed before any untrusted command
/// runs.
struct WslHelperInvocation {
    /// Absolute path of `bwrap` inside WSL (located by the Windows-side probe).
    bwrap_path: PathBuf,
    /// Pre-built bwrap *options* — everything before the trailing `-- cmd` —
    /// assembled on the Windows side (`windows_wsl::build_bwrap_args`): root
    /// bind, `/tmp` tmpfs, writable binds, Windows-interop blocking, `--setenv`s,
    /// `--chdir`, namespace flags, etc.
    base_args: Vec<OsString>,
    /// The writable bind destinations to validate — exactly the ones `base_args`
    /// binds read-write. Each is captured here (WSL-side) and checked in-sandbox.
    writable_paths: Vec<PathBuf>,
    program: OsString,
    args: Vec<OsString>,
}

/// Handle a possible re-exec of this binary as the WSL-side sandbox helper. Does
/// not return if it was invoked as one.
pub fn run_wsl_helper_if_invoked() {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).and_then(|arg| arg.to_str()) == Some(WSL_RESOLVE_FLAG) {
        let Some(path) = args.get(2) else {
            eprintln!("zed: malformed WSL canonical-path resolver invocation");
            std::process::exit(127);
        };
        if args.len() != 3 {
            eprintln!("zed: malformed WSL canonical-path resolver invocation");
            std::process::exit(127);
        }
        match crate::util::CanonicalPathBuf::resolve(PathBuf::from(path.as_os_str())) {
            Ok(canonical) => {
                let Some(canonical) = canonical.path().to_str() else {
                    eprintln!("zed: canonical WSL sandbox grant is not valid UTF-8");
                    std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
                };
                let Ok(distro) = std::env::var("WSL_DISTRO_NAME") else {
                    eprintln!("zed: WSL_DISTRO_NAME is unavailable while resolving sandbox grant");
                    std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
                };
                // Classify the backing filesystem of the *canonical* target (the
                // path that will actually be bound), so the Windows side can
                // persist whether this grant lives on a Windows-hosted (DrvFs)
                // filesystem, whose sandbox-integrity guarantees are weaker.
                let fs_class = if path_is_on_windows_fs(Path::new(canonical)) {
                    "windows-fs"
                } else {
                    "native-fs"
                };
                println!("{distro}");
                println!("{canonical}");
                println!("{fs_class}");
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("zed: could not resolve WSL sandbox grant: {error}");
                std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
            }
        }
    }

    let Some(invocation) = parse_wsl_helper_args(args) else {
        return;
    };
    let invocation = match invocation {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("zed: malformed WSL sandbox helper invocation: {error:#}");
            std::process::exit(127);
        }
    };
    run_wsl_helper(invocation);
}

/// Whether `path` resides on a Windows-hosted filesystem exposed to WSL via
/// DrvFs. Inside WSL the Windows drives (`/mnt/<letter>`) are served by either
/// 9p or virtiofs, whereas the distro's own filesystems are native (ext4/btrfs/
/// overlay/tmpfs). We use the backing filesystem type rather than a `/mnt/`
/// path prefix so a reconfigured automount root, or a symlink that lands on a
/// Windows drive, is still classified correctly. Fails safe: if the type can't
/// be determined we report `true` (weaker guarantees ⇒ warn) rather than
/// silently treating it as native.
fn path_is_on_windows_fs(path: &Path) -> bool {
    // `statfs.f_type` magics for the DrvFs transports. virtiofs is FUSE-backed.
    const V9FS_MAGIC: u64 = 0x0102_1997;
    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;
    match nix::sys::statfs::statfs(path) {
        Ok(stat) => {
            // `f_type` is `i64` on glibc but `u64` on musl, so widen it
            // losslessly instead of comparing against a fixed-width constant.
            u64::try_from(stat.filesystem_type().0)
                .is_ok_and(|fs_type| fs_type == V9FS_MAGIC || fs_type == FUSE_SUPER_MAGIC)
        }
        Err(_) => true,
    }
}

fn parse_wsl_helper_args(
    args: impl IntoIterator<Item = OsString>,
) -> Option<Result<WslHelperInvocation>> {
    let mut args = args.into_iter();
    args.next()?;
    if args.next()?.to_str() != Some(WSL_HELPER_FLAG) {
        return None;
    }
    Some(decode_wsl_helper_args(args))
}

fn decode_wsl_helper_args(mut args: impl Iterator<Item = OsString>) -> Result<WslHelperInvocation> {
    let bwrap_path = PathBuf::from(args.next().context("missing bwrap path")?);
    let base_count = parse_count(
        args.next().context("missing base-arg count")?,
        "base-arg count",
    )?;
    let mut base_args = Vec::with_capacity(base_count);
    for _ in 0..base_count {
        base_args.push(args.next().context("missing base arg")?);
    }
    let writable_count = parse_count(
        args.next().context("missing writable count")?,
        "writable count",
    )?;
    if writable_count > MAX_VALIDATED_BINDS {
        bail!("writable count {writable_count} exceeds {MAX_VALIDATED_BINDS}");
    }
    let mut writable_paths = Vec::with_capacity(writable_count);
    for _ in 0..writable_count {
        writable_paths.push(PathBuf::from(args.next().context("missing writable path")?));
    }
    let separator = args.next().context("missing argument separator")?;
    if separator != "--" {
        bail!("missing argument separator");
    }
    let program = args.next().context("missing program to run")?;
    let args = args.collect();
    Ok(WslHelperInvocation {
        bwrap_path,
        base_args,
        writable_paths,
        program,
        args,
    })
}

fn parse_count(value: OsString, what: &str) -> Result<usize> {
    value
        .to_str()
        .with_context(|| format!("{what} is not valid UTF-8"))?
        .parse::<usize>()
        .with_context(|| format!("invalid {what}"))
}

/// The WSL-side helper entry point. Mirrors the native-Linux host side: capture
/// the writable binds' inodes (here, inside WSL), stand up the validation socket,
/// finish assembling the bwrap invocation (validation socket bind + in-sandbox
/// validator re-exec), spawn bwrap, and propagate its exit. Never returns.
#[allow(
    clippy::disallowed_methods,
    reason = "the WSL helper is a dedicated per-command process that must spawn and wait for bwrap"
)]
fn run_wsl_helper(invocation: WslHelperInvocation) -> ! {
    let mut fds = Vec::with_capacity(invocation.writable_paths.len());
    for path in &invocation.writable_paths {
        let fd = crate::util::CanonicalPathBuf::from_canonical(path.clone())
            .and_then(|canonical| canonical.dup_fd());
        match fd {
            Ok(fd) => fds.push(fd),
            Err(error) => {
                eprintln!(
                    "zed: WSL sandbox helper could not verify canonical writable bind {}: {error}",
                    path.display()
                );
                std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
            }
        }
    }

    let validation = if fds.is_empty() {
        None
    } else {
        match ValidationFdSender::spawn(fds) {
            Ok(sender) => Some(sender),
            Err(error) => {
                eprintln!("zed: WSL sandbox helper could not start the bind validator: {error}");
                std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
            }
        }
    };

    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("zed: WSL sandbox helper could not resolve its own path: {error}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    };

    let mut args = invocation.base_args.clone();
    // Always re-exec ourselves as the in-sandbox launcher, even when there is
    // nothing to validate: the launcher is where the seccomp filter is installed
    // on the untrusted command (see `exec_command`). When there are writable
    // binds, also bind the validation socket so the launcher can verify them.
    // WSL has no restricted-network bridge, so both bridge fields are the absent
    // sentinel.
    if let Some(sender) = &validation {
        // Bind the validation socket in, after the base args' tmpfs and writable
        // binds so it isn't shadowed.
        args.push(OsString::from("--bind"));
        args.push(sender.host_socket_path().as_os_str().to_os_string());
        args.push(sender.sandbox_socket_path().as_os_str().to_os_string());
    }
    args.push(OsString::from("--"));
    args.push(current_exe.into_os_string());
    args.push(OsString::from(LAUNCHER_FLAG));
    // Field 1: validation socket (in-sandbox path) or sentinel.
    match &validation {
        Some(sender) => args.push(sender.sandbox_socket_path().as_os_str().to_os_string()),
        None => args.push(OsString::from(LAUNCHER_NONE)),
    }
    // Fields 2-3: bridge socket + port (WSL has no bridge).
    args.push(OsString::from(LAUNCHER_NONE));
    args.push(OsString::from(LAUNCHER_NONE));
    // Field 4: writable bind-destination paths to validate (count, then paths);
    // empty when there is nothing to validate.
    let validation_paths: &[PathBuf] = if validation.is_some() {
        &invocation.writable_paths
    } else {
        &[]
    };
    args.push(OsString::from(validation_paths.len().to_string()));
    for path in validation_paths {
        args.push(path.clone().into_os_string());
    }
    args.push(OsString::from("--"));
    args.push(invocation.program.clone());
    args.extend(invocation.args.iter().cloned());

    let mut child = match Command::new(&invocation.bwrap_path).args(&args).spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("zed: WSL sandbox helper could not spawn bwrap: {error}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    };

    let status = child.wait();
    // Hold the validator open until bwrap (and thus the in-sandbox validator that
    // connects to it during startup) has finished.
    drop(validation);
    match status {
        Ok(status) => {
            if let Some(code) = status.code() {
                std::process::exit(code);
            }
            let signal = status.signal().unwrap_or(1);
            std::process::exit(128 + signal);
        }
        Err(error) => {
            eprintln!("zed: WSL sandbox helper failed waiting for bwrap: {error}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    }
}

fn run_bridge_listener(listener: TcpListener, socket_path: PathBuf) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let socket_path = socket_path.clone();
                if let Err(error) = thread::Builder::new()
                    .name("zed-sandbox-bridge-conn".to_string())
                    .stack_size(128 * 1024)
                    .spawn(move || forward_bridge_connection(stream, socket_path))
                {
                    eprintln!("zed: failed to spawn sandbox bridge connection thread: {error}");
                }
            }
            Err(error) => eprintln!("zed: sandbox bridge accept failed: {error}"),
        }
    }
}

fn forward_bridge_connection(tcp_stream: TcpStream, socket_path: PathBuf) {
    let unix_stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!(
                "zed: sandbox bridge failed to connect to proxy socket {}: {error}",
                socket_path.display()
            );
            return;
        }
    };
    copy_bidirectional(tcp_stream, unix_stream);
}

fn copy_bidirectional(tcp_stream: TcpStream, unix_stream: UnixStream) {
    let tcp_read = match tcp_stream.try_clone() {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("zed: sandbox bridge failed to clone TCP stream: {error}");
            return;
        }
    };
    let unix_read = match unix_stream.try_clone() {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("zed: sandbox bridge failed to clone Unix stream: {error}");
            return;
        }
    };

    let tcp_write = tcp_stream;
    let unix_write = unix_stream;
    let to_proxy = match thread::Builder::new()
        .name("zed-sandbox-bridge-out".to_string())
        .stack_size(128 * 1024)
        .spawn(move || copy_one_way(tcp_read, unix_write))
    {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("zed: failed to spawn sandbox bridge pump thread: {error}");
            return;
        }
    };
    copy_one_way(unix_read, tcp_write);
    if to_proxy.join().is_err() {
        eprintln!("zed: sandbox bridge pump thread panicked");
    }
}

trait BridgeStream: Read + Write {
    fn shutdown(&self, how: Shutdown) -> std::io::Result<()>;
}

impl BridgeStream for TcpStream {
    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        TcpStream::shutdown(self, how)
    }
}

impl BridgeStream for UnixStream {
    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        UnixStream::shutdown(self, how)
    }
}

fn copy_one_way(mut from: impl Read, mut to: impl BridgeStream) {
    let mut buffer = vec![0; PUMP_BUFFER_SIZE];
    loop {
        let count = match from.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) => break,
        };
        if to.write_all(&buffer[..count]).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

