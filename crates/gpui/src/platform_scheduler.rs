use crate::{PlatformDispatcher, RunnableMeta};
use async_task::Runnable;
use chrono::{DateTime, Utc};
use futures::channel::oneshot;
use scheduler::Instant;
use scheduler::{
  Clock, LocalExecutor, Priority, Scheduler, SessionId, Task, Timer, spawn_dedicated_thread,
};
use std::task::{Context, Poll};
use std::{
  any::Any,
  future::Future,
  pin::Pin,
  sync::{
    Arc,
    atomic::{AtomicU16, Ordering},
  },
  time::Duration,
};

/// A production implementation of [`Scheduler`] that wraps a [`PlatformDispatcher`].
///
/// This allows GPUI to use the scheduler crate's executor types with the platform's
/// native dispatch mechanisms (e.g., Grand Central Dispatch on macOS).
pub struct PlatformScheduler {
  dispatcher: Arc<dyn PlatformDispatcher>,
  clock: Arc<PlatformClock>,
  next_session_id: AtomicU16,
  #[cfg(feature = "profiler")]
  foreground_runnables: crate::profiler::journal::ForegroundRunnableCounter,
}

impl PlatformScheduler {
  pub fn new(dispatcher: Arc<dyn PlatformDispatcher>) -> Self {
    Self {
      dispatcher: dispatcher.clone(),
      clock: Arc::new(PlatformClock { dispatcher }),
      next_session_id: AtomicU16::new(0),
      #[cfg(feature = "profiler")]
      foreground_runnables: crate::profiler::journal::foreground_runnable_counter(),
    }
  }

  pub fn foreground_executor(self: &Arc<Self>) -> LocalExecutor {
    let session_id = self.next_session_id();
    let scheduler = Arc::downgrade(self);
    LocalExecutor::new(session_id, self.clone(), move |runnable| {
      if let Some(scheduler) = scheduler.upgrade() {
        scheduler.schedule_local(session_id, runnable);
      }
    })
  }

  fn next_session_id(&self) -> SessionId {
    SessionId::new(self.next_session_id.fetch_add(1, Ordering::SeqCst))
  }

  #[cfg(feature = "profiler")]
  pub(crate) fn foreground_runnable_counter(
    &self,
  ) -> crate::profiler::journal::ForegroundRunnableCounter {
    self.foreground_runnables.clone()
  }
}

impl Scheduler for PlatformScheduler {
  #[cfg(not(target_family = "wasm"))]
  fn block(
    &self,
    _session_id: Option<SessionId>,
    mut future: Pin<&mut dyn Future<Output = ()>>,
    timeout: Option<Duration>,
  ) -> bool {
    use waker_fn::waker_fn;
    let deadline = timeout.map(|t| Instant::now() + t);
    let parker = parking::Parker::new();
    let unparker = parker.unparker();
    let waker = waker_fn(move || {
      unparker.unpark();
    });
    let mut cx = Context::from_waker(&waker);
    if let Poll::Ready(()) = future.as_mut().poll(&mut cx) {
      return true;
    }

    let park_deadline = |deadline: Instant| {
      // Timer expirations are only delivered every ~15.6 milliseconds by default on Windows.
      // We increase the resolution during this wait so that short timeouts stay reasonably short.
      let _timer_guard = self.dispatcher.increase_timer_resolution();
      parker.park_deadline(deadline)
    };

    loop {
      match deadline {
        Some(deadline) if !park_deadline(deadline) && deadline <= Instant::now() => {
          return false;
        }
        Some(_) => (),
        None => parker.park(),
      }
      if let Poll::Ready(()) = future.as_mut().poll(&mut cx) {
        break true;
      }
    }
  }

  fn schedule_local(&self, _session_id: SessionId, runnable: Runnable<RunnableMeta>) {
    #[cfg(feature = "profiler")]
    self.foreground_runnables.queued();
    self
      .dispatcher
      .dispatch_on_main_thread(runnable, Priority::default());
  }

  fn schedule_background_with_priority(
    &self,
    runnable: Runnable<RunnableMeta>,
    priority: Priority,
  ) {
    self.dispatcher.dispatch(runnable, priority);
  }

  fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
    self.dispatcher.spawn_realtime(f);
  }

  #[track_caller]
  fn timer(&self, duration: Duration) -> Timer {
    let (tx, rx) = oneshot::channel();
    let dispatcher = self.dispatcher.clone();

    // Create a runnable that will send the completion signal
    let location = std::panic::Location::caller();
    let (runnable, _task) = async_task::Builder::new()
      .metadata(RunnableMeta {
        location,
        spawned: scheduler::SpawnTime(Instant::now()),
      })
      .spawn(
        move |_| async move {
          let _ = tx.send(());
        },
        move |runnable| {
          dispatcher.dispatch_after(duration, runnable);
        },
      );
    runnable.schedule();

    Timer::new(rx)
  }

  fn clock(&self) -> Arc<dyn Clock> {
    self.clock.clone()
  }

  fn spawn_dedicated(
    self: Arc<Self>,
    f: Box<
      dyn FnOnce(LocalExecutor) -> Pin<Box<dyn Future<Output = Box<dyn Any + Send + Sync>> + 'static>>
        + Send
        + 'static,
    >,
  ) -> Task<Box<dyn Any + Send + Sync>> {
    let session_id = self.next_session_id();
    spawn_dedicated_thread(session_id, self, move |executor| f(executor))
  }
}

/// A production clock that uses the platform dispatcher's time.
struct PlatformClock {
  dispatcher: Arc<dyn PlatformDispatcher>,
}

impl Clock for PlatformClock {
  fn utc_now(&self) -> DateTime<Utc> {
    Utc::now()
  }

  fn now(&self) -> Instant {
    self.dispatcher.now()
  }
}
