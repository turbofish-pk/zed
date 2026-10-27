use std::{path::PathBuf, rc::Rc};

use gpui::{
  AnyWindowHandle, ClipboardItem, CursorStyle, GraphicalEnvironment, PlatformDisplay,
  PlatformKeyboardLayout, PlatformWindow, WindowParams,
};

use super::WaylandConnection;

/// The Wayland connection a `LinuxPlatform` uses.
///
/// - **Attach** (`WaylandConnection::attach`): connect, create the connection's state, then
///   register its event sources on the platform's loop. If a step fails, dropping the partial
///   connection undoes the earlier ones.
/// - **Detach** (drop): removes the event sources, stops the desktop portal listener and closes
///   the connection. The platform takes the connection out of its `RefCell` before dropping it,
///   because dropping can call back into the platform.
///
/// What outlives the connection, in `LinuxCommon`, is session-independent: the executors, the
/// text system, callbacks and the power listener.
pub(crate) enum DisplayConnection {
  Wayland(WaylandConnection),
}

impl DisplayConnection {
  fn wayland(&self) -> &WaylandConnection {
    let Self::Wayland(connection) = self;
    connection
  }

  pub(crate) fn compositor_name(&self) -> &'static str {
    self.wayland().compositor_name()
  }

  /// The environment this connection was made in.
  pub(crate) fn graphical_environment(&self) -> Option<GraphicalEnvironment> {
    Some(self.wayland().graphical_environment())
  }

  pub(crate) fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
    self.wayland().keyboard_layout()
  }

  pub(crate) fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
    self.wayland().displays()
  }

  pub(crate) fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
    self.wayland().primary_display()
  }

  pub(crate) fn open_window(
    &self,
    handle: AnyWindowHandle,
    params: WindowParams,
  ) -> anyhow::Result<Box<dyn PlatformWindow>> {
    self.wayland().open_window(handle, params)
  }

  pub(crate) fn active_window(&self) -> Option<AnyWindowHandle> {
    self.wayland().active_window()
  }

  pub(crate) fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
    self.wayland().window_stack()
  }

  pub(crate) fn set_cursor_style(&self, style: CursorStyle) {
    self.wayland().set_cursor_style(style)
  }

  pub(crate) fn hide_cursor_until_mouse_moves(&self) {
    self.wayland().hide_cursor_until_mouse_moves()
  }

  pub(crate) fn is_cursor_visible(&self) -> bool {
    self.wayland().is_cursor_visible()
  }

  pub(crate) fn open_uri(&self, uri: &str) {
    self.wayland().open_uri(uri)
  }

  pub(crate) fn reveal_path(&self, path: PathBuf) {
    self.wayland().reveal_path(path)
  }

  pub(crate) fn write_to_primary(&self, item: ClipboardItem) {
    self.wayland().write_to_primary(item)
  }

  pub(crate) fn write_to_clipboard(&self, item: ClipboardItem) {
    self.wayland().write_to_clipboard(item)
  }

  pub(crate) fn read_from_primary(&self) -> Option<ClipboardItem> {
    self.wayland().read_from_primary()
  }

  pub(crate) fn read_from_clipboard(&self) -> Option<ClipboardItem> {
    self.wayland().read_from_clipboard()
  }

  pub(crate) fn window_identifier(
    &self,
  ) -> futures::future::BoxFuture<'static, Option<ashpd::WindowIdentifier>> {
    Box::pin(self.wayland().window_identifier())
  }
}
