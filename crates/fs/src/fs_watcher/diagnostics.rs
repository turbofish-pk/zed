use super::{Coverage, OsWatcher, OsWatcherKind};
use parking_lot::Mutex;
use serde::Serialize;
use std::{
    cell::LazyCell,
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const EVENT_CAPACITY: usize = 10_000;
const RESCAN_PATH_HISTORY_CAPACITY: usize = 10;

/// An opt-in recording. Watchers hold only weak references to its buffer, so
/// dropping this value stops collection without changing any watches.
pub struct WatchRecording {
    state: Arc<Mutex<RecordingState>>,
    watchers: Vec<Arc<OsWatcher>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WatchSnapshot {
    pub started_at_unix_millis: u128,
    pub captured_at_unix_millis: u128,
    pub capacity: usize,
    pub dropped_events: u64,
    pub watchers: Vec<WatcherSnapshot>,
    pub events: Vec<Arc<WatchDiagnosticEvent>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WatcherSnapshot {
    pub backend: OsWatcherKind,
    pub recursive: bool,
    pub roots: Vec<WatchRoot>,
    pub cooldown_remaining_millis: Option<u128>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WatchRoot {
    pub path: String,
    /// Zero when the backend watch is retained only for covered descendants.
    pub registrations: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct WatchDiagnosticEvent {
    pub timestamp_unix_millis: u128,
    pub backend: OsWatcherKind,
    pub operation: String,
    pub event_kind: Option<String>,
    /// Paths are displayed lossily so a non-UTF-8 filename cannot prevent export.
    pub paths: Vec<String>,
    pub detail: String,
    pub rescan: bool,
}

impl WatchDiagnosticEvent {
    pub(super) fn new(
        backend: OsWatcherKind,
        operation: &str,
        paths: &[PathBuf],
        detail: String,
        rescan: bool,
    ) -> Self {
        Self {
            timestamp_unix_millis: unix_millis(),
            backend,
            operation: operation.into(),
            event_kind: None,
            paths: paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            detail,
            rescan,
        }
    }

    pub(super) fn from_notify_event(backend: OsWatcherKind, event: &notify::Event) -> Self {
        Self {
            event_kind: Some(format!("{:?}", event.kind)),
            ..Self::new(
                backend,
                "event",
                &event.paths,
                format!("{:?} {:?}", event.kind, event.attrs),
                event.need_rescan(),
            )
        }
    }
}

struct RecordingState {
    started_at_unix_millis: u128,
    dropped_events: u64,
    events: VecDeque<Arc<WatchDiagnosticEvent>>,
}

#[derive(Default)]
pub(super) struct DiagnosticRecorder {
    recordings: Mutex<Vec<Weak<Mutex<RecordingState>>>>,
}

impl DiagnosticRecorder {
    pub(super) fn record(&self, event: impl FnOnce() -> WatchDiagnosticEvent) {
        let mut recordings = self.recordings.lock();
        // Formatting raw events and allocating paths must not happen when
        // there is no debug window recording them.
        let event = LazyCell::new(|| Arc::new(event()));
        recordings.retain(|recording| {
            let Some(recording) = recording.upgrade() else {
                return false;
            };
            let mut recording = recording.lock();
            if recording.events.len() == EVENT_CAPACITY {
                recording.events.pop_front();
                recording.dropped_events += 1;
            }
            recording.events.push_back((*event).clone());
            true
        });
    }
}

#[derive(Default)]
pub(super) struct RescanHistory {
    paths: VecDeque<PathBuf>,
}

impl RescanHistory {
    pub(super) fn record(&mut self, event: &notify::Event) -> Option<String> {
        let report = event.need_rescan().then(|| {
            // Capture before inserting the rescan so the report includes all ten
            // preceding paths, even when this is the start of an overflow burst.
            format!(
                "reason={:?}, paths={:?}, recent_paths={:?}",
                event.info().unwrap_or("unspecified"),
                event.paths,
                self.paths,
            )
        });
        let first_path = event
            .paths
            .len()
            .saturating_sub(RESCAN_PATH_HISTORY_CAPACITY);
        for path in event.paths.iter().skip(first_path) {
            if self.paths.len() == RESCAN_PATH_HISTORY_CAPACITY {
                self.paths.pop_front();
            }
            self.paths.push_back(path.clone());
        }
        report
    }
}

impl WatchRecording {
    pub(crate) fn new(watchers: impl IntoIterator<Item = Arc<OsWatcher>>) -> Self {
        let state = Arc::new(Mutex::new(RecordingState {
            started_at_unix_millis: unix_millis(),
            dropped_events: 0,
            events: VecDeque::new(),
        }));
        let watchers: Vec<_> = watchers.into_iter().collect();
        for watcher in &watchers {
            let mut recordings = watcher.diagnostics.recordings.lock();
            recordings.retain(|recording| recording.strong_count() > 0);
            recordings.push(Arc::downgrade(&state));
        }
        Self { state, watchers }
    }

    pub fn snapshot(&self) -> WatchSnapshot {
        let (started_at_unix_millis, dropped_events, events) = {
            let state = self.state.lock();
            (
                state.started_at_unix_millis,
                state.dropped_events,
                state.events.iter().cloned().collect(),
            )
        };
        let watchers = self
            .watchers
            .iter()
            .map(|watcher| {
                let state = watcher.state.lock();
                let mut roots: Vec<_> = state
                    .paths
                    .0
                    .values()
                    .filter(|path| matches!(path.coverage, Coverage::Watched { .. }))
                    .map(|path| WatchRoot {
                        path: path.path.as_path().to_string_lossy().into_owned(),
                        registrations: path.watcher_ids.len(),
                    })
                    .collect();
                roots.sort_by(|left, right| left.path.cmp(&right.path));
                WatcherSnapshot {
                    backend: watcher.kind,
                    recursive: watcher.recursive,
                    roots,
                    cooldown_remaining_millis: state
                        .cooldown_until
                        .and_then(|until| until.checked_duration_since(Instant::now()))
                        .map(|remaining| remaining.as_millis()),
                }
            })
            .collect();
        WatchSnapshot {
            started_at_unix_millis,
            captured_at_unix_millis: unix_millis(),
            capacity: EVENT_CAPACITY,
            dropped_events,
            watchers,
            events,
        }
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

