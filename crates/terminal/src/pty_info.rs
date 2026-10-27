use gpui::{Context, Task};
use parking_lot::{MappedRwLockReadGuard, Mutex, RwLock, RwLockReadGuard};
use std::{path::PathBuf, sync::Arc};

#[cfg(target_os = "windows")]
use windows::Win32::{Foundation::HANDLE, System::Threading::GetProcessId};

use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::{Event, Terminal};

#[derive(Clone, Copy)]
pub struct ProcessIdGetter {
    handle: i32,
    fallback_pid: u32,
}

impl ProcessIdGetter {
    pub(crate) fn new(handle: i32, fallback_pid: u32) -> ProcessIdGetter {
        ProcessIdGetter {
            handle,
            fallback_pid,
        }
    }

    pub fn fallback_pid(&self) -> Pid {
        Pid::from_u32(self.fallback_pid)
    }
}

impl ProcessIdGetter {
    fn pid(&self) -> Option<Pid> {
        // Negative pid means error.
        // Zero pid means no foreground process group is set on the PTY yet.
        // Avoid killing the current process by returning a zero pid.
        let pid = unsafe { libc::tcgetpgrp(self.handle) };
        if pid > 0 {
            return Some(Pid::from_u32(pid as u32));
        }

        if self.fallback_pid > 0 {
            return Some(Pid::from_u32(self.fallback_pid));
        }

        None
    }
}

#[cfg(windows)]
impl ProcessIdGetter {
    fn pid(&self) -> Option<Pid> {
        let pid = unsafe { GetProcessId(HANDLE(self.handle as _)) };
        // the GetProcessId may fail and returns zero, which will lead to a stack overflow issue
        if pid == 0 {
            // in the builder process, there is a small chance, almost negligible,
            // that this value could be zero, which means child_watcher returns None,
            // GetProcessId returns 0.
            if self.fallback_pid == 0 {
                return None;
            }
            return Some(Pid::from_u32(self.fallback_pid));
        }
        Some(Pid::from_u32(pid))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessInfo {
    pub(crate) name: String,
    pub(crate) cwd: PathBuf,
    pub(crate) argv: Vec<String>,
}

/// Fetches Zed-relevant Pseudo-Terminal (PTY) process information
pub(crate) struct PtyProcessInfo {
    system: RwLock<System>,
    refresh_kind: ProcessRefreshKind,
    pid_getter: ProcessIdGetter,
    last_foreground_pid: Mutex<Option<Pid>>,
    pub(crate) current: RwLock<Option<ProcessInfo>>,
    task: Mutex<Option<Task<()>>>,
}

impl PtyProcessInfo {
    pub(crate) fn new(pid_getter: ProcessIdGetter) -> PtyProcessInfo {
        // Task enumeration is on by default and would retain a `Process` entry
        // per thread, each pinning an open `/proc/<pid>/task/<tid>/stat` handle
        // on Linux (#58651).
        let process_refresh_kind = ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .without_tasks();
        // `System::new_with_specifics` with a process refresh kind would
        // snapshot every process on the machine into this terminal's `System`,
        // retaining one open procfs handle per process for the lifetime of the
        // terminal (#58651). Refresh only the spawned child so that
        // `kill_child_process` works before the first foreground refresh.
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid_getter.fallback_pid()]),
            true,
            process_refresh_kind,
        );

        PtyProcessInfo {
            system: RwLock::new(system),
            refresh_kind: process_refresh_kind,
            pid_getter,
            last_foreground_pid: Mutex::new(None),
            current: RwLock::new(None),
            task: Mutex::new(None),
        }
    }

    pub(crate) fn pid_getter(&self) -> &ProcessIdGetter {
        &self.pid_getter
    }

    fn refresh(&self) -> Option<MappedRwLockReadGuard<'_, Process>> {
        let pid = self.pid_getter.pid()?;
        let fallback_pid = self.pid_getter.fallback_pid();
        let mut system = self.system.write();
        // sysinfo never evicts processes that are absent from the refreshed pid
        // set, so entries for former foreground processes (each pinning an open
        // `/proc/<pid>/stat` handle on Linux) would otherwise accumulate for as
        // long as this terminal lives (#58651). Rebuild the `System` whenever
        // the foreground process changes to keep the map bounded.
        if self.last_foreground_pid.lock().replace(pid) != Some(pid) {
            *system = System::new();
        }
        let pids = [pid, fallback_pid];
        let pids = if pid == fallback_pid {
            &pids[..1]
        } else {
            &pids[..]
        };
        system.refresh_processes_specifics(ProcessesToUpdate::Some(pids), true, self.refresh_kind);
        drop(system);
        RwLockReadGuard::try_map(self.system.read(), |system| system.process(pid)).ok()
    }

    fn get_child(&self) -> Option<MappedRwLockReadGuard<'_, Process>> {
        let pid = self.pid_getter.fallback_pid();
        RwLockReadGuard::try_map(self.system.read(), |system| system.process(pid)).ok()
    }

    pub(crate) fn kill_current_process(&self) -> bool {
        let Some(pid) = self.pid_getter.pid() else {
            return false;
        };
        unsafe { libc::killpg(pid.as_u32() as i32, libc::SIGKILL) == 0 }
    }

    #[cfg(not(unix))]
    pub(crate) fn kill_current_process(&self) -> bool {
        self.refresh().is_some_and(|process| process.kill())
    }

    pub(crate) fn kill_child_process(&self) -> bool {
        self.get_child().is_some_and(|process| process.kill())
    }

    pub(crate) fn terminate_child_process(&self) -> bool {
        let pid = self.pid_getter.fallback_pid();
        unsafe { libc::killpg(pid.as_u32() as i32, libc::SIGTERM) == 0 }
    }

    #[cfg(not(unix))]
    pub(crate) fn terminate_child_process(&self) -> bool {
        false
    }

    fn load(&self) -> Option<ProcessInfo> {
        let process = self.refresh()?;
        let cwd = process.cwd().map_or(PathBuf::new(), |p| p.to_owned());

        let info = ProcessInfo {
            name: process.name().to_str()?.to_owned(),
            cwd,
            argv: process
                .cmd()
                .iter()
                .filter_map(|s| s.to_str().map(ToOwned::to_owned))
                .collect(),
        };
        *self.current.write() = Some(info.clone());
        Some(info)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn load_for_test(&self) -> Option<ProcessInfo> {
        self.load()
    }

    /// Updates the cached process info, emitting a [`Event::TitleChanged`] event if the Zed-relevant info has changed
    pub(crate) fn emit_title_changed_if_changed(self: &Arc<Self>, cx: &mut Context<'_, Terminal>) {
        if self.task.lock().is_some() {
            return;
        }
        let this = self.clone();
        let change_task = cx.background_executor().spawn(async move {
            let previous = this.current.read().clone();
            let current = this.load();
            let has_changed = match (previous.as_ref(), current.as_ref()) {
                (None, None) => false,
                (Some(prev), Some(now)) => prev.cwd != now.cwd || prev.name != now.name,
                _ => true,
            };
            if has_changed {
                *this.current.write() = current.clone();
            }
            let changed_cwd = match (previous.as_ref(), current.as_ref()) {
                (Some(prev), Some(now)) if prev.cwd != now.cwd => Some(now.cwd.clone()),
                (None, Some(now)) => Some(now.cwd.clone()),
                _ => None,
            };
            (has_changed, changed_cwd)
        });
        let this = Arc::downgrade(self);
        *self.task.lock() = Some(cx.spawn(async move |term, cx| {
            let (has_changed, new_cwd) = change_task.await;
            if has_changed {
                term.update(cx, |terminal, cx| {
                    if let Some(cwd) = new_cwd {
                        terminal.record_cwd_change(cwd);
                    }
                    cx.emit(Event::TitleChanged);
                })
                .ok();
            }
            if let Some(this) = this.upgrade() {
                this.task.lock().take();
            }
        }));
    }

    pub(crate) fn pid(&self) -> Option<Pid> {
        self.pid_getter.pid()
    }
}
