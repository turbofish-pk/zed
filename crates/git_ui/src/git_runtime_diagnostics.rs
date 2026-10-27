//! Best-effort runtime diagnostics gathered to accompany the git job queue
//! debug dump. Every individual step is fallible and any failure is logged at
//! `warn` level and silently omitted from the output. The dump itself must
//! never fail just because diagnostics couldn't be collected.
//!
//! What we gather:
//! - All transitive descendant processes of the current Zed process
//!   (cross-platform via `sysinfo`).
//! - On Linux: each descendant's `/proc/<pid>/wchan` (kernel function the
//!   thread is currently sleeping in) and `State:` from `/proc/<pid>/status`.
//! - On macOS: for any descendant whose executable name contains `git`, a
//!   short `sample`-based user-space stack and `lsof` output. Both require
//!   the corresponding system binaries; if they aren't present or the
//!   invocation fails we skip them.
//! - On Windows: just the process tree (no portable way to grab another
//!   process's stack).
//!
//! The output is a `serde_json::Value`. Callers merge it into whatever larger
//! JSON payload they're producing.
//!
//! This is invoked from a developer-only "show git job queue" action, so it
//! is acceptable for the macOS path to spend a few seconds sampling.

use serde_json::{Map, Value};

pub async fn gather() -> Value {
    let mut diag = Map::new();

    match collect_process_tree() {
        Ok(tree) => {
            diag.insert("processes".into(), tree);
        }
        Err(err) => {
            log::warn!("git runtime diagnostics: failed to collect process tree: {err:#}");
        }
    }

    
    
        match collect_linux_proc_info() {
            Ok(info) => {
                diag.insert("linux_proc".into(), info);
            }
            Err(err) => {
                log::warn!("git runtime diagnostics: failed to read /proc info: {err:#}");
            }
        }
    

    Value::Object(diag)
}

/// Walk the descendant tree of the current process and return a JSON array
/// describing each descendant. Cross-platform; uses `sysinfo`.
fn collect_process_tree() -> anyhow::Result<Value> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

    let current_pid = sysinfo::get_current_pid()
        .map_err(|e| anyhow::anyhow!("sysinfo::get_current_pid failed: {e}"))?;

    let refresh = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always);
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

    let descendants = descendants_of(&system, current_pid);

    let entries: Vec<Value> = descendants
        .iter()
        .filter_map(|pid| {
            let process = system.process(*pid)?;
            let cmd = sanitize_cmd(
                process
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned()),
            );
            Some(serde_json::json!({
                "pid": pid.as_u32(),
                "ppid": process.parent().map(|p| p.as_u32()),
                "name": process.name().to_string_lossy(),
                "exe": process.exe().map(|p| p.display().to_string()),
                "cmd": cmd,
                "status": format!("{:?}", process.status()),
                "run_time_secs": process.run_time(),
            }))
        })
        .collect();

    Ok(serde_json::json!({
        "zed_pid": current_pid.as_u32(),
        "descendant_count": entries.len(),
        "descendants": entries,
    }))
}

/// Scrub a process's reported argv to avoid leaking environment-variable
/// values. sysinfo's `Process::cmd()` on macOS goes through `KERN_PROCARGS2`
/// and can include envp in addition to argv for some processes, which means
/// the raw output can contain things like `ANTHROPIC_API_KEY=…`. We replace
/// any entry that matches a conservative env-var pattern (uppercase
/// identifier ending in `=`) with `KEY=<redacted>`. If *every* entry got
/// redacted then sysinfo's data for this process is too garbled to trust as
/// argv, so we return `None` so the caller emits a JSON null rather than
/// something misleading.
fn sanitize_cmd(cmd: impl IntoIterator<Item = String>) -> Option<Vec<String>> {
    let sanitized: Vec<String> = cmd.into_iter().map(redact_env_var_entry).collect();
    if sanitized.is_empty() {
        return None;
    }
    let all_redacted = sanitized.iter().all(|s| s.ends_with("=<redacted>"));
    if all_redacted { None } else { Some(sanitized) }
}

fn redact_env_var_entry(entry: String) -> String {
    // Match `IDENT=...` where IDENT is at least two characters starting with
    // an uppercase letter or underscore and otherwise uppercase/digit/under.
    // CLI flags (`--foo=bar`, `-x=y`, `/path=value`) don't match.
    let Some(eq_index) = entry.find('=') else {
        return entry;
    };
    let key = &entry[..eq_index];
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && key.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
    {
        format!("{key}=<redacted>")
    } else {
        entry
    }
}


fn descendants_of(system: &sysinfo::System, root: sysinfo::Pid) -> Vec<sysinfo::Pid> {
    let mut parent_map: std::collections::HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> =
        std::collections::HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            parent_map.entry(parent).or_default().push(*pid);
        }
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some(children) = parent_map.get(&p) {
            for c in children {
                out.push(*c);
                stack.push(*c);
            }
        }
    }
    out
}


fn collect_linux_proc_info() -> anyhow::Result<Value> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    let current_pid = sysinfo::get_current_pid()
        .map_err(|e| anyhow::anyhow!("sysinfo::get_current_pid failed: {e}"))?;
    let refresh = ProcessRefreshKind::nothing();
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

    let descendants = descendants_of(&system, current_pid);

    let mut entries = Vec::new();
    for pid in descendants {
        let pid_u32 = pid.as_u32();

        let wchan = match std::fs::read_to_string(format!("/proc/{pid_u32}/wchan")) {
            Ok(s) => Value::String(s.trim().to_string()),
            Err(err) => {
                log::warn!("git runtime diagnostics: failed to read /proc/{pid_u32}/wchan: {err}");
                Value::Null
            }
        };

        let state = match std::fs::read_to_string(format!("/proc/{pid_u32}/status")) {
            Ok(contents) => contents
                .lines()
                .find(|l| l.starts_with("State:"))
                .map(|l| Value::String(l.trim_start_matches("State:").trim().to_string()))
                .unwrap_or(Value::Null),
            Err(err) => {
                log::warn!("git runtime diagnostics: failed to read /proc/{pid_u32}/status: {err}");
                Value::Null
            }
        };

        entries.push(serde_json::json!({
            "pid": pid_u32,
            "wchan": wchan,
            "state": state,
        }));
    }

    Ok(serde_json::json!({ "descendants": entries }))
}
