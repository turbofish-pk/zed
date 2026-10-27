mod dispatcher;
mod display_connection;
mod keyboard;
mod platform;
mod system_notifications;
mod text_system;
mod wayland;
mod xdg_desktop_portal;

pub use dispatcher::*;
pub(crate) use display_connection::DisplayConnection;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
pub(crate) use text_system::*;
pub(crate) use wayland::*;

use std::{rc::Rc, sync::Arc};

use gpui::WindowingModes;

/// Returns the default platform implementation for the current OS.
///
/// This build supports Wayland only and has no headless mode.
///
/// # Panics
///
/// Panics if a headless platform is requested, by `headless` or `ZED_HEADLESS`.
pub fn current_platform(headless: bool) -> Rc<dyn gpui::Platform> {
  assert!(
    !headless && std::env::var_os("ZED_HEADLESS").is_none(),
    "this build of gpui_linux has no headless mode"
  );
  linux_platform(WindowingModes::all())
}

/// Returns a platform connected to the Wayland compositor the process environment names.
///
/// `allowed_modes` is ignored: only Wayland is supported. Connect to another environment with
/// [`gpui::Application::with_windowing`].
pub fn linux_platform(allowed_modes: WindowingModes) -> Rc<dyn gpui::Platform> {
  Rc::new(LinuxPlatform::new(allowed_modes))
}

/// Returns a background executor that needs no display connection.
///
/// It has no main-thread event loop, so work dispatched to the main thread is dropped. Only
/// background work runs, as with a headless platform that was never run.
pub fn background_executor() -> gpui::BackgroundExecutor {
  let (main_sender, _main_receiver) = PriorityQueueCalloopReceiver::new();
  gpui::BackgroundExecutor::new(Arc::new(LinuxDispatcher::new(main_sender)))
}
