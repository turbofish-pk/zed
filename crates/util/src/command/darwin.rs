use mach2::exception_types::{
    EXC_MASK_ALL, EXCEPTION_DEFAULT, exception_behavior_t, exception_mask_t,
};
use mach2::port::{MACH_PORT_NULL, mach_port_t};
use mach2::thread_status::{THREAD_STATE_NONE, thread_state_flavor_t};
use smol::Async;
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::ptr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stdio {
    /// A new pipe should be arranged to connect the parent and child processes.
    #[default]
    Piped,
    /// The child inherits from the corresponding parent descriptor.
    Inherit,
    /// This stream will be ignored (redirected to `/dev/null`).
    Null,
}

impl Stdio {
    pub fn piped() -> Self {
        Self::Piped
    }

    pub fn inherit() -> Self {
        Self::Inherit
    }

    pub fn null() -> Self {
        Self::Null
    }
}

unsafe extern "C" {
    fn posix_spawnattr_setexceptionports_np(
        attr: *mut libc::posix_spawnattr_t,
        mask: exception_mask_t,
        new_port: mach_port_t,
        behavior: exception_behavior_t,
        new_flavor: thread_state_flavor_t,
    ) -> libc::c_int;

    fn posix_spawn_file_actions_addchdir_np(
        file_actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;

    fn posix_spawn_file_actions_addinherit_np(
        file_actions: *mut libc::posix_spawn_file_actions_t,
        filedes: libc::c_int,
    ) -> libc::c_int;

    static environ: *const *mut libc::c_char;
}

#[derive(Debug)]
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    envs: BTreeMap<OsString, Option<OsString>>,
    env_clear: bool,
    current_dir: Option<PathBuf>,
    stdin_cfg: Option<Stdio>,
    stdout_cfg: Option<Stdio>,
    stderr_cfg: Option<Stdio>,
    kill_on_drop: bool,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            envs: BTreeMap::new(),
            env_clear: false,
            current_dir: None,
            stdin_cfg: None,
            stdout_cfg: None,
            stderr_cfg: None,
            kill_on_drop: false,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(|s| s.as_os_str())
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.envs
            .insert(key.as_ref().to_owned(), Some(val.as_ref().to_owned()));
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, val) in vars {
            self.envs
                .insert(key.as_ref().to_owned(), Some(val.as_ref().to_owned()));
        }
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref().to_owned();
        if self.env_clear {
            self.envs.remove(&key);
        } else {
            self.envs.insert(key, None);
        }
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.env_clear = true;
        self.envs.clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    pub fn stdin(&mut self, cfg: Stdio) -> &mut Self {
        self.stdin_cfg = Some(cfg);
        self
    }

    pub fn stdout(&mut self, cfg: Stdio) -> &mut Self {
        self.stdout_cfg = Some(cfg);
        self
    }

    pub fn stderr(&mut self, cfg: Stdio) -> &mut Self {
        self.stderr_cfg = Some(cfg);
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        let current_dir = self
            .current_dir
            .as_deref()
            .unwrap_or_else(|| Path::new("."));

        // Optimization: if no environment modifications were requested, pass None
        // to spawn_posix so it uses the `environ` global directly, avoiding a
        // full copy of the environment. This matches std::process::Command behavior.
        let envs = if self.env_clear || !self.envs.is_empty() {
            let mut result = BTreeMap::<OsString, OsString>::new();
            if !self.env_clear {
                for (key, val) in std::env::vars_os() {
                    result.insert(key, val);
                }
            }
            for (key, maybe_val) in &self.envs {
                if let Some(val) = maybe_val {
                    result.insert(key.clone(), val.clone());
                } else {
                    result.remove(key);
                }
            }
            Some(result.into_iter().collect::<Vec<_>>())
        } else {
            None
        };

        spawn_posix_spawn(
            &self.program,
            &self.args,
            current_dir,
            envs.as_deref(),
            self.stdin_cfg.unwrap_or_default(),
            self.stdout_cfg.unwrap_or_default(),
            self.stderr_cfg.unwrap_or_default(),
            self.kill_on_drop,
        )
    }

    pub async fn output(&mut self) -> io::Result<Output> {
        self.stdin_cfg.get_or_insert(Stdio::null());
        self.stdout_cfg.get_or_insert(Stdio::piped());
        self.stderr_cfg.get_or_insert(Stdio::piped());

        let child = self.spawn()?;
        child.output().await
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        let mut child = self.spawn()?;
        child.status().await
    }

    pub fn get_program(&self) -> &OsStr {
        self.program.as_os_str()
    }
}

#[derive(Debug)]
pub struct Child {
    inner: smol::process::Child,
    pub stdin: Option<Async<std::fs::File>>,
    pub stdout: Option<Async<std::fs::File>>,
    pub stderr: Option<Async<std::fs::File>>,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.inner.kill()
    }

    pub fn try_status(&mut self) -> io::Result<Option<ExitStatus>> {
        self.inner.try_status()
    }

    pub fn status(
        &mut self,
    ) -> impl std::future::Future<Output = io::Result<ExitStatus>> + Send + 'static {
        self.stdin.take();
        self.inner.status()
    }

    pub async fn output(mut self) -> io::Result<Output> {
        use futures_lite::AsyncReadExt;

        let status = self.status();

        let stdout = self.stdout.take();
        let stdout_future = async move {
            let mut data = Vec::new();
            if let Some(mut stdout) = stdout {
                stdout.read_to_end(&mut data).await?;
            }
            io::Result::Ok(data)
        };

        let stderr = self.stderr.take();
        let stderr_future = async move {
            let mut data = Vec::new();
            if let Some(mut stderr) = stderr {
                stderr.read_to_end(&mut data).await?;
            }
            io::Result::Ok(data)
        };

        let (stdout_data, stderr_data) =
            futures_lite::future::try_zip(stdout_future, stderr_future).await?;
        let status = status.await?;

        Ok(Output {
            status,
            stdout: stdout_data,
            stderr: stderr_data,
        })
    }
}

fn spawn_posix_spawn(
    program: &OsStr,
    args: &[OsString],
    current_dir: &Path,
    envs: Option<&[(OsString, OsString)]>,
    stdin_cfg: Stdio,
    stdout_cfg: Stdio,
    stderr_cfg: Stdio,
    kill_on_drop: bool,
) -> io::Result<Child> {
    // posix_spawnp resolves programs against the parent's cwd/PATH, not the child's.
    let resolved_program = if program.as_bytes().contains(&b'/') {
        std::path::absolute(current_dir.join(program)).map_or_else(
            |_| program.as_bytes().to_vec(),
            |p| p.into_os_string().into_vec(),
        )
    } else {
        envs.and_then(|e| {
            e.iter()
                .find(|(k, _)| k.as_os_str() == OsStr::new("PATH"))
                .and_then(|(_, v)| which::which_in(program, Some(v.as_os_str()), current_dir).ok())
        })
        .map_or_else(
            || program.as_bytes().to_vec(),
            |path| path.into_os_string().into_vec(),
        )
    };
    let program_cstr = CString::new(resolved_program).map_err(|_| invalid_input_error())?;
    let argv0_cstr = CString::new(program.as_bytes()).map_err(|_| invalid_input_error())?;

    let current_dir_cstr =
        CString::new(current_dir.as_os_str().as_bytes()).map_err(|_| invalid_input_error())?;

    let mut argv_cstrs = vec![argv0_cstr];
    for arg in args {
        let cstr = CString::new(arg.as_bytes()).map_err(|_| invalid_input_error())?;
        argv_cstrs.push(cstr);
    }
    let mut argv_ptrs: Vec<*mut libc::c_char> = argv_cstrs
        .iter()
        .map(|s| s.as_ptr() as *mut libc::c_char)
        .collect();
    argv_ptrs.push(ptr::null_mut());

    let envp: Vec<CString> = if let Some(envs) = envs {
        envs.iter()
            .map(|(key, value)| {
                let mut env_str = key.as_bytes().to_vec();
                env_str.push(b'=');
                env_str.extend_from_slice(value.as_bytes());
                CString::new(env_str)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| invalid_input_error())?
    } else {
        Vec::new()
    };
    let mut envp_ptrs: Vec<*mut libc::c_char> = envp
        .iter()
        .map(|s| s.as_ptr() as *mut libc::c_char)
        .collect();
    envp_ptrs.push(ptr::null_mut());

    let (stdin_read, stdin_write) = match stdin_cfg {
        Stdio::Piped => {
            let (r, w) = create_pipe()?;
            (Some(r), Some(w))
        }
        Stdio::Null => {
            let fd = open_dev_null(libc::O_RDONLY)?;
            (Some(fd), None)
        }
        Stdio::Inherit => (None, None),
    };

    let (stdout_read, stdout_write) = match stdout_cfg {
        Stdio::Piped => {
            let (r, w) = create_pipe()?;
            (Some(r), Some(w))
        }
        Stdio::Null => {
            let fd = open_dev_null(libc::O_WRONLY)?;
            (None, Some(fd))
        }
        Stdio::Inherit => (None, None),
    };

    let (stderr_read, stderr_write) = match stderr_cfg {
        Stdio::Piped => {
            let (r, w) = create_pipe()?;
            (Some(r), Some(w))
        }
        Stdio::Null => {
            let fd = open_dev_null(libc::O_WRONLY)?;
            (None, Some(fd))
        }
        Stdio::Inherit => (None, None),
    };

    let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
    let mut file_actions: libc::posix_spawn_file_actions_t = ptr::null_mut();

    unsafe {
        cvt_nz(libc::posix_spawnattr_init(&mut attr))?;
        cvt_nz(libc::posix_spawn_file_actions_init(&mut file_actions))?;

        // The Rust runtime sets SIGPIPE to SIG_IGN before `main`, and ignored
        // dispositions survive exec, so without this children would never die
        // from writing to a closed pipe. Reset it to SIG_DFL, like std does
        // (rust-lang/rust#101077). Like std, we don't touch the signal mask,
        // so deliberately blocked signals (e.g. via `nohup`) stay blocked.
        let mut default_set: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut default_set) == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::sigaddset(&mut default_set, libc::SIGPIPE) == -1 {
            return Err(io::Error::last_os_error());
        }
        cvt_nz(libc::posix_spawnattr_setsigdefault(&mut attr, &default_set))?;

        cvt_nz(libc::posix_spawnattr_setflags(
            &mut attr,
            (libc::POSIX_SPAWN_CLOEXEC_DEFAULT | libc::POSIX_SPAWN_SETSIGDEF) as libc::c_short,
        ))?;

        cvt_nz(posix_spawnattr_setexceptionports_np(
            &mut attr,
            EXC_MASK_ALL,
            MACH_PORT_NULL,
            EXCEPTION_DEFAULT as exception_behavior_t,
            THREAD_STATE_NONE,
        ))?;

        cvt_nz(posix_spawn_file_actions_addchdir_np(
            &mut file_actions,
            current_dir_cstr.as_ptr(),
        ))?;

        // With POSIX_SPAWN_CLOEXEC_DEFAULT, any fd without a file action is
        // closed in the child, so inheriting a stdio fd requires an explicit
        // addinherit_np action; without one the child's fd 0/1/2 would start
        // out closed and could be silently reused by its first open().
        if let Some(fd) = &stdin_read {
            cvt_nz(libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                fd.as_raw_fd(),
                libc::STDIN_FILENO,
            ))?;
        }
        if stdin_read.is_some() || stdin_cfg == Stdio::Inherit {
            cvt_nz(posix_spawn_file_actions_addinherit_np(
                &mut file_actions,
                libc::STDIN_FILENO,
            ))?;
        }

        if let Some(fd) = &stdout_write {
            cvt_nz(libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                fd.as_raw_fd(),
                libc::STDOUT_FILENO,
            ))?;
        }
        if stdout_write.is_some() || stdout_cfg == Stdio::Inherit {
            cvt_nz(posix_spawn_file_actions_addinherit_np(
                &mut file_actions,
                libc::STDOUT_FILENO,
            ))?;
        }

        if let Some(fd) = &stderr_write {
            cvt_nz(libc::posix_spawn_file_actions_adddup2(
                &mut file_actions,
                fd.as_raw_fd(),
                libc::STDERR_FILENO,
            ))?;
        }
        if stderr_write.is_some() || stderr_cfg == Stdio::Inherit {
            cvt_nz(posix_spawn_file_actions_addinherit_np(
                &mut file_actions,
                libc::STDERR_FILENO,
            ))?;
        }

        let mut pid: libc::pid_t = 0;

        let spawn_result = libc::posix_spawnp(
            &mut pid,
            program_cstr.as_ptr(),
            &file_actions,
            &attr,
            argv_ptrs.as_ptr(),
            if envs.is_some() {
                envp_ptrs.as_ptr()
            } else {
                environ
            },
        );

        libc::posix_spawnattr_destroy(&mut attr);
        libc::posix_spawn_file_actions_destroy(&mut file_actions);

        cvt_nz(spawn_result)?;

        let inner = smol::process::Child::adopt_raw_pid(pid as u32, true, kill_on_drop)?;

        Ok(Child {
            inner,
            stdin: stdin_write.map(Async::new).transpose()?,
            stdout: stdout_read.map(Async::new).transpose()?,
            stderr: stderr_read.map(Async::new).transpose()?,
        })
    }
}

fn create_pipe() -> io::Result<(std::fs::File, std::fs::File)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    unsafe {
        let result = libc::pipe(fds.as_mut_ptr());
        if result == -1 {
            let error = io::Error::last_os_error();
            return Err(error);
        }

        // Set close-on-exec on both ends of the pipe.
        //
        // Without this, unrelated spawns elsewhere in the process (e.g.
        // `smol::process` or `async_process`, which on Apple platforms use
        // `posix_spawn` *without* `POSIX_SPAWN_CLOEXEC_DEFAULT`) would inherit
        // these file descriptors and keep the pipes open even after we drop our
        // side.
        for &fd in &fds {
            let result = libc::ioctl(fd, libc::FIOCLEX);
            if result == -1 {
                let error = io::Error::last_os_error();
                libc::close(fds[0]);
                libc::close(fds[1]);
                return Err(error);
            }
        }

        Ok((
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        ))
    }
}

fn open_dev_null(flags: libc::c_int) -> io::Result<std::fs::File> {
    // Set close-on-exec for this pipe, for the same reason as in `create_pipe`.
    let fd = unsafe {
        libc::open(
            c"/dev/null".as_ptr() as *const libc::c_char,
            flags | libc::O_CLOEXEC,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Zero means `Ok()`, all other values are treated as raw OS errors. Does not look at `errno`.
/// Mirrored after Rust's std `cvt_nz` function.
fn cvt_nz(error: libc::c_int) -> io::Result<()> {
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error))
    }
}

fn invalid_input_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid argument: path or argument contains null byte",
    )
}

