use std::{
  cell::RefCell,
  env,
  ffi::OsString,
  fs::File,
  io::Read as _,
  os::fd::{AsFd, AsRawFd},
  path::{Path, PathBuf},
  rc::Rc,
  sync::Arc,
  time::Duration,
};

use anyhow::{Context as _, anyhow, ensure};
use ashpd::{
  desktop::{
    Request,
    inhibit::{InhibitFlags, InhibitOptions, InhibitProxy},
  },
  enumflags2::BitFlags,
};
use calloop::{EventLoop, LoopHandle, LoopSignal, channel::Sender};
use futures::channel::oneshot;
use gpui::{
  Action, ActivityGuard, AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle,
  ForegroundExecutor, GraphicalEnvironment, Keymap, Menu, MenuItem, OwnedMenu, PathPromptOptions,
  Pixels, Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper,
  PlatformTextSystem, PlatformWindow, Point, Result, RunnableVariant, Task, ThermalState,
  WindowAppearance, WindowButtonLayout, WindowParams, WindowingModes, WindowingRequest, px,
};
use gpui_util::{ResultExt as _, new_std_command};
use xkbcommon::xkb::{self, Keycode, Keysym, State};

use crate::linux::{DisplayConnection, LinuxDispatcher, PriorityQueueCalloopReceiver};

pub(crate) const SCROLL_LINES: f32 = 3.0;

// Values match the defaults on GTK.
// Taken from https://github.com/GNOME/gtk/blob/main/gtk/gtksettings.c#L320
pub(crate) const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
pub(crate) const DOUBLE_CLICK_DISTANCE: Pixels = px(5.0);

pub(crate) const KEYRING_LABEL: &str = "zed-github-account";

const FILE_PICKER_PORTAL_MISSING: &str =
  "Couldn't open file picker due to missing xdg-desktop-portal implementation.";

type Callback = Option<Box<dyn FnMut()>>;

#[derive(Default)]
pub(crate) struct PlatformHandlers {
  pub(crate) open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
  pub(crate) quit: Option<Box<dyn FnMut() -> bool>>,
  pub(crate) reopen: Callback,
  pub(crate) app_menu_action: Option<Box<dyn FnMut(&dyn Action)>>,
  pub(crate) will_open_app_menu: Callback,
  pub(crate) validate_app_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
  pub(crate) keyboard_layout_change: Callback,
  pub(crate) system_sleep: Callback,
  pub(crate) system_wake: Callback,
  pub(crate) displays_changed: Callback,
}

/// A logind `PrepareForSleep` signal, forwarded from the D-Bus listener to
/// the client's event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemPowerEvent {
  Sleep,
  Wake,
}

pub(crate) struct LinuxCommon {
  pub(crate) background_executor: BackgroundExecutor,
  pub(crate) foreground_executor: ForegroundExecutor,
  pub(crate) text_system: Arc<dyn PlatformTextSystem>,
  pub(crate) appearance: WindowAppearance,
  pub(crate) auto_hide_scrollbars: bool,
  pub(crate) button_layout: WindowButtonLayout,
  pub(crate) callbacks: PlatformHandlers,
  pub(crate) signal: LoopSignal,
  pub(crate) menus: Vec<OwnedMenu>,
  app_name: Option<String>,
  system_notifications: crate::linux::system_notifications::SystemNotificationState,
  power_sender: Sender<SystemPowerEvent>,
  power_listener_started: bool,
}

impl LinuxCommon {
  pub fn new(
    signal: LoopSignal,
  ) -> (
    Self,
    PriorityQueueCalloopReceiver<RunnableVariant>,
    calloop::channel::Channel<SystemPowerEvent>,
  ) {
    let (main_sender, main_receiver) = PriorityQueueCalloopReceiver::new();
    let (power_sender, power_receiver) = calloop::channel::channel();
    let dispatcher = Arc::new(LinuxDispatcher::new(main_sender));

    let common = LinuxCommon {
      background_executor: BackgroundExecutor::new(dispatcher.clone()),
      foreground_executor: ForegroundExecutor::new(dispatcher),
      text_system: Arc::new(crate::linux::CosmicTextSystem::new("IBM Plex Sans")),
      appearance: WindowAppearance::Light,
      auto_hide_scrollbars: false,
      button_layout: WindowButtonLayout::linux_default(),
      callbacks: PlatformHandlers::default(),
      signal,
      menus: Vec::new(),
      app_name: None,
      system_notifications: crate::linux::system_notifications::SystemNotificationState::new(),
      power_sender,
      power_listener_started: false,
    };

    (common, main_receiver, power_receiver)
  }

  pub(crate) fn start_power_listener(&mut self) {
    if self.power_listener_started {
      return;
    }
    let power_sender = self.power_sender.clone();
    smol::spawn(async move {
      if let Err(error) = listen_for_system_power_events(power_sender).await {
        log::debug!("failed to listen for system sleep/wake events: {error:?}");
      }
    })
    .detach();
    self.power_listener_started = true;
  }

  /// Registers the foreground executor and power listener on the event loop.
  ///
  /// Foreground runnables run as idle callbacks, after the loop has dispatched all pending events.
  pub(crate) fn register_sources(
    common: &Rc<RefCell<Self>>,
    loop_handle: &LoopHandle<'static, ()>,
    main_receiver: PriorityQueueCalloopReceiver<RunnableVariant>,
    power_receiver: calloop::channel::Channel<SystemPowerEvent>,
  ) -> anyhow::Result<()> {
    loop_handle
      .insert_source(main_receiver, {
        let loop_handle = loop_handle.clone();
        move |event, _, _| {
          if let calloop::channel::Event::Msg(runnable) = event {
            loop_handle.insert_idle(move |_| {
              let location = runnable.metadata().location;
              let spawned = runnable.metadata().spawned;
              gpui::profiler::update_running_task(spawned, location);
              runnable.run();
              gpui::profiler::save_task_timing();
            });
          }
        }
      })
      .map_err(|error| anyhow!("failed to register foreground executor: {error}"))?;
    loop_handle
      .insert_source(power_receiver, {
        let common = common.clone();
        move |event, _, _| {
          if let calloop::channel::Event::Msg(event) = event {
            Self::run_callback(&common, |callbacks| match event {
              SystemPowerEvent::Sleep => &mut callbacks.system_sleep,
              SystemPowerEvent::Wake => &mut callbacks.system_wake,
            });
          }
        }
      })
      .map_err(|error| anyhow!("failed to register power listener: {error}"))?;
    Ok(())
  }

  /// Runs the callback in `slot` without `common` borrowed, since the callback can call back
  /// into the platform. Puts it back afterwards, unless it replaced itself.
  fn run_callback(
    common: &RefCell<Self>,
    slot: impl Fn(&mut PlatformHandlers) -> &mut Callback,
  ) {
    let callback = slot(&mut common.borrow_mut().callbacks).take();
    if let Some(mut callback) = callback {
      callback();
      slot(&mut common.borrow_mut().callbacks).get_or_insert(callback);
    }
  }
}

async fn listen_for_system_power_events(
  power_sender: Sender<SystemPowerEvent>,
) -> anyhow::Result<()> {
  use futures::StreamExt as _;

  let connection = ashpd::zbus::Connection::system().await?;
  let proxy = ashpd::zbus::Proxy::new(
    &connection,
    "org.freedesktop.login1",
    "/org/freedesktop/login1",
    "org.freedesktop.login1.Manager",
  )
  .await?;
  let mut sleep_events = proxy.receive_signal("PrepareForSleep").await?;

  while let Some(message) = sleep_events.next().await {
    let event = if message.body().deserialize::<bool>()? {
      SystemPowerEvent::Sleep
    } else {
      SystemPowerEvent::Wake
    };
    power_sender.send(event).ok();
  }

  Ok(())
}

/// GPUI's platform for Linux, on Wayland only.
///
/// One event loop serves the whole process. The foreground executor and power listener are
/// registered on it once, and the Wayland connection adds its own sources.
pub(crate) struct LinuxPlatform {
  /// Declared first so it is dropped before the event loop and `common`.
  connection: RefCell<DisplayConnection>,
  common: Rc<RefCell<LinuxCommon>>,
  /// The event loop, until `run` takes it.
  event_loop: RefCell<Option<EventLoop<'static, ()>>>,
  loop_handle: LoopHandle<'static, ()>,
  /// An environment to reconnect to when `run` starts, if the app asked for one.
  initial_windowing: RefCell<Option<WindowingRequest>>,
}

/// What the Wayland connection made at startup may use from this process's environment.
///
/// Both are taken from the environment when the platform is created, before it starts any
/// threads, and are only meaningful for the connection made then.
#[derive(Default)]
struct StartupEnvironment {
  activation_token: Option<String>,
  /// The socket `WAYLAND_SOCKET` names: a connection to the compositor that the process which
  /// started us already opened.
  wayland_socket: Option<std::os::unix::net::UnixStream>,
}

impl LinuxPlatform {
  /// Creates the platform and connects to the Wayland compositor named by the environment.
  ///
  /// # Panics
  ///
  /// Panics if the compositor can't be reached, since the app can't start.
  pub(crate) fn new(_allowed_modes: WindowingModes) -> Self {
    let startup = StartupEnvironment {
      activation_token: crate::linux::take_startup_activation_token_from_environment(),
      wayland_socket: crate::linux::take_wayland_socket_from_environment(),
    };
    let event_loop = EventLoop::try_new().expect("failed to create Linux event loop");
    let (common, main_receiver, power_receiver) = LinuxCommon::new(event_loop.get_signal());
    let common = Rc::new(RefCell::new(common));
    let loop_handle = event_loop.handle();
    LinuxCommon::register_sources(&common, &loop_handle, main_receiver, power_receiver)
      .expect("failed to register Linux event sources");
    let connection = Self::connect(
      &loop_handle,
      &common,
      &GraphicalEnvironment::detect(),
      startup,
    )
    .unwrap_or_else(|error| panic!("{error:#}"));

    Self {
      connection: RefCell::new(connection),
      common,
      event_loop: RefCell::new(Some(event_loop)),
      loop_handle,
      initial_windowing: RefCell::new(None),
    }
  }

  fn with_common<R>(&self, function: impl FnOnce(&mut LinuxCommon) -> R) -> R {
    function(&mut self.common.borrow_mut())
  }

  /// Connects to the Wayland compositor in `environment`.
  ///
  /// `startup` is only given for the connection made when the platform is created.
  fn connect(
    loop_handle: &LoopHandle<'static, ()>,
    common: &Rc<RefCell<LinuxCommon>>,
    environment: &GraphicalEnvironment,
    startup: StartupEnvironment,
  ) -> anyhow::Result<DisplayConnection> {
    let activation_token = environment
      .activation_token
      .clone()
      .or(startup.activation_token);
    Ok(DisplayConnection::Wayland(
      crate::linux::WaylandConnection::attach(
        loop_handle.clone(),
        common.clone(),
        environment,
        startup.wayland_socket,
        activation_token,
      )?,
    ))
  }

  /// Installs `connection`, then drops the previous one outside the borrow, since dropping a
  /// connection can call back into the platform.
  fn replace_connection(&self, connection: DisplayConnection) {
    let previous = std::mem::replace(&mut *self.connection.borrow_mut(), connection);
    drop(previous);
    // The app caches the keyboard layout and the displays, which belong to the connection.
    LinuxCommon::run_callback(&self.common, |callbacks| {
      &mut callbacks.keyboard_layout_change
    });
    LinuxCommon::run_callback(&self.common, |callbacks| &mut callbacks.displays_changed);
  }
}

impl Platform for LinuxPlatform {
  fn platform_info(&self) -> gpui::PlatformInfo {
    gpui::PlatformInfo {
      os: gpui::OperatingSystem::Linux,
      target: gpui::PlatformTarget::Native,
    }
  }

  fn background_executor(&self) -> BackgroundExecutor {
    self.with_common(|common| common.background_executor.clone())
  }

  fn foreground_executor(&self) -> ForegroundExecutor {
    self.with_common(|common| common.foreground_executor.clone())
  }

  fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
    self.with_common(|common| common.text_system.clone())
  }

  fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
    self.connection.borrow().keyboard_layout()
  }

  fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
    Rc::new(gpui::DummyKeyboardMapper)
  }

  fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
    self.with_common(|common| common.callbacks.keyboard_layout_change = Some(callback));
  }

  fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

  fn thermal_state(&self) -> ThermalState {
    ThermalState::Nominal
  }

  fn prevent_idle_sleep(&self, reason: &str) -> Task<Result<ActivityGuard>> {
    let executor = self.background_executor();
    let (guard_tx, guard_rx) = oneshot::channel();
    executor
      .spawn({
        let executor = executor.clone();
        let reason = reason.to_owned();
        async move {
          guard_tx
            .send(inhibit_idle_sleep(reason, executor).await)
            .ok();
        }
      })
      .detach();
    executor
      .clone()
      .spawn(async move { await_idle_sleep_prevention(guard_rx, &executor).await })
  }

  fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
    let request = self.initial_windowing.borrow_mut().take();
    if let Some(request) = request {
      let WindowingRequest::Windowed(environment) = request;
      let connection = Self::connect(
        &self.loop_handle,
        &self.common,
        &environment,
        StartupEnvironment::default(),
      )
      .unwrap_or_else(|error| panic!("{error:#}"));
      self.replace_connection(connection);
    }
    on_finish_launching();

    let mut event_loop = self
      .event_loop
      .borrow_mut()
      .take()
      .expect("App is already running");
    event_loop.run(None, &mut (), |_| {}).log_err();

    let quit = self.with_common(|common| common.callbacks.quit.take());
    if let Some(mut fun) = quit {
      fun();
    }
  }

  fn quit(&self) {
    self.with_common(|common| common.signal.stop());
  }

  fn set_initial_windowing(&self, request: WindowingRequest) {
    *self.initial_windowing.borrow_mut() = Some(request);
  }

  fn request_windowing(&self, _request: WindowingRequest) -> Task<anyhow::Result<()>> {
    Task::ready(Err(anyhow!(
      "already windowed ({}); switching display modes is not supported",
      self.compositor_name()
    )))
  }

  fn compositor_name(&self) -> &'static str {
    self.connection.borrow().compositor_name()
  }

  fn graphical_environment(&self) -> Option<GraphicalEnvironment> {
    self.connection.borrow().graphical_environment()
  }

  fn restart(&self, binary_path: Option<PathBuf>, arguments: Vec<OsString>) {
    use std::os::unix::process::CommandExt as _;

    let app_pid = std::process::id().to_string();
    let app_path = match binary_path {
      Some(path) => path,
      None => match self.app_path() {
        Ok(path) => path,
        Err(err) => {
          log::error!("Failed to get app path: {:?}", err);
          return;
        }
      },
    };

    log::info!("Restarting process, using app path: {:?}", app_path);

    // Script to wait for the current process to exit and then restart the app.
    // Pass dynamic values as positional parameters to avoid shell interpolation issues.
    let script = r#"
            while kill -0 "$0" 2>/dev/null; do
                sleep 0.1
            done

            app_path="$1"
            shift
            "$app_path" "$@"
            "#;

    #[allow(
      clippy::disallowed_methods,
      reason = "We are restarting ourselves, using std command thus is fine"
    )]
    let restart_process = new_std_command("/usr/bin/env")
      .arg("bash")
      .arg("-c")
      .arg(script)
      .arg(&app_pid)
      .arg(&app_path)
      .args(arguments)
      .process_group(0)
      .spawn();

    match restart_process {
      Ok(_) => self.quit(),
      Err(e) => log::error!("failed to spawn restart script: {:?}", e),
    }
  }

  fn activate(&self, _ignoring_other_apps: bool) {
    log::info!("activate is not implemented on Linux, ignoring the call")
  }

  fn hide(&self) {
    log::info!("hide is not implemented on Linux, ignoring the call")
  }

  fn hide_other_apps(&self) {
    log::info!("hide_other_apps is not implemented on Linux, ignoring the call")
  }

  fn unhide_other_apps(&self) {
    log::info!("unhide_other_apps is not implemented on Linux, ignoring the call")
  }

  fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
    self.connection.borrow().primary_display()
  }

  fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
    self.connection.borrow().displays()
  }

  fn on_displays_changed(&self, callback: Box<dyn FnMut()>) {
    self.with_common(|common| common.callbacks.displays_changed = Some(callback));
  }

  fn active_window(&self) -> Option<AnyWindowHandle> {
    self.connection.borrow().active_window()
  }

  fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
    self.connection.borrow().window_stack()
  }

  fn open_window(
    &self,
    handle: AnyWindowHandle,
    options: WindowParams,
  ) -> anyhow::Result<Box<dyn PlatformWindow>> {
    self.connection.borrow().open_window(handle, options)
  }

  fn open_url(&self, url: &str) {
    self.connection.borrow().open_uri(url);
  }

  fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
    self.with_common(|common| common.callbacks.open_urls = Some(callback));
  }

  fn prompt_for_paths(
    &self,
    options: PathPromptOptions,
  ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
    let (done_tx, done_rx) = oneshot::channel();
    let identifier = self.connection.borrow().window_identifier();

    self
      .foreground_executor()
      .spawn(async move {
        let title = if options.directories {
          "Open Folder"
        } else {
          "Open File"
        };

        let request = match ashpd::desktop::file_chooser::OpenFileRequest::default()
          .identifier(identifier.await)
          .modal(true)
          .title(title)
          .accept_label(options.prompt.as_ref().map(gpui::SharedString::as_str))
          .multiple(options.multiple)
          .directory(options.directories)
          .send()
          .await
        {
          Ok(request) => request,
          Err(err) => {
            let _ = done_tx.send(Err(portal_error(err)));
            return;
          }
        };

        let result = match request.response() {
          Ok(response) => Ok(Some(
            response
              .uris()
              .iter()
              .filter_map(|uri: &ashpd::Uri| url::Url::parse(uri.as_str()).ok())
              .filter_map(|uri: url::Url| uri.to_file_path().ok())
              .collect::<Vec<_>>(),
          )),
          Err(ashpd::Error::Response(_)) => Ok(None),
          Err(e) => Err(e.into()),
        };
        let _ = done_tx.send(result);
      })
      .detach();
    done_rx
  }

  fn prompt_for_new_path(
    &self,
    directory: &Path,
    suggested_name: Option<&str>,
  ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
    let (done_tx, done_rx) = oneshot::channel();
    let identifier = self.connection.borrow().window_identifier();
    let directory = directory.to_owned();
    let suggested_name = suggested_name.map(str::to_owned);

    self
      .foreground_executor()
      .spawn(async move {
        let mut request_builder = ashpd::desktop::file_chooser::SaveFileRequest::default()
          .identifier(identifier.await)
          .modal(true)
          .title("Save File")
          .current_folder(directory)
          .expect("pathbuf should not be nul terminated");

        if let Some(suggested_name) = suggested_name {
          request_builder = request_builder.current_name(suggested_name.as_str());
        }

        let request = match request_builder.send().await {
          Ok(request) => request,
          Err(err) => {
            let _ = done_tx.send(Err(portal_error(err)));
            return;
          }
        };

        let result = match request.response() {
          Ok(response) => Ok(
            response
              .uris()
              .first()
              .and_then(|uri: &ashpd::Uri| url::Url::parse(uri.as_str()).ok())
              .and_then(|uri: url::Url| uri.to_file_path().ok()),
          ),
          Err(ashpd::Error::Response(_)) => Ok(None),
          Err(e) => Err(e.into()),
        };
        let _ = done_tx.send(result);
      })
      .detach();

    done_rx
  }

  fn can_select_mixed_files_and_dirs(&self) -> bool {
    // org.freedesktop.portal.FileChooser only supports "pick files" and "pick directories".
    false
  }

  fn reveal_path(&self, path: &Path) {
    self.connection.borrow().reveal_path(path.to_owned());
  }

  fn open_with_system(&self, path: &Path) {
    let path = path.to_owned();
    let environment = self.connection.borrow().graphical_environment();
    self
      .background_executor()
      .spawn(async move {
        let mut command = new_std_command("xdg-open");
        command.arg(path);
        if let Some(environment) = environment {
          environment.apply_to(&mut command);
        }
        #[allow(
          clippy::disallowed_methods,
          reason = "running on a background thread, so blocking is fine"
        )]
        command.status().context("invoking xdg-open").log_err();
      })
      .detach();
  }

  fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
    self.with_common(|common| common.callbacks.quit = Some(callback));
  }

  fn on_reopen(&self, callback: Box<dyn FnMut()>) {
    self.with_common(|common| common.callbacks.reopen = Some(callback));
  }

  fn on_system_sleep(&self, callback: Box<dyn FnMut()>) {
    self.with_common(|common| {
      common.callbacks.system_sleep = Some(callback);
      common.start_power_listener();
    });
  }

  fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
    self.with_common(|common| {
      common.callbacks.system_wake = Some(callback);
      common.start_power_listener();
    });
  }

  fn set_app_identity(&self, _identifier: &str, name: &str) {
    self.with_common(|common| common.app_name = Some(name.to_string()));
  }

  fn show_system_notification(&self, notification: gpui::SystemNotification) {
    self.with_common(|common| {
      common
        .system_notifications
        .show(common.app_name.as_deref(), notification)
    });
  }

  fn dismiss_system_notification(&self, tag: &str) {
    self.with_common(|common| common.system_notifications.dismiss(tag));
  }

  fn on_system_notification_response(
    &self,
    callback: Box<dyn FnMut(gpui::SystemNotificationResponse)>,
  ) {
    self.with_common(|common| {
      let executor = common.foreground_executor.clone();
      common.system_notifications.on_response(&executor, callback)
    });
  }

  fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
    self.with_common(|common| common.callbacks.app_menu_action = Some(callback));
  }

  fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
    self.with_common(|common| common.callbacks.will_open_app_menu = Some(callback));
  }

  fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
    self.with_common(|common| common.callbacks.validate_app_menu_command = Some(callback));
  }

  fn app_path(&self) -> Result<PathBuf> {
    Ok(env::current_exe()?)
  }

  fn set_menus(&self, menus: Vec<Menu>, _keymap: &Keymap) {
    self.with_common(|common| {
      common.menus = menus.into_iter().map(|menu| menu.owned()).collect();
    })
  }

  fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
    self.with_common(|common| Some(common.menus.clone()))
  }

  fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {
    // todo(linux)
  }

  fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
    Err(anyhow::Error::msg(
      "Platform<LinuxPlatform>::path_for_auxiliary_executable is not implemented yet",
    ))
  }

  fn set_cursor_style(&self, style: CursorStyle) {
    self.connection.borrow().set_cursor_style(style)
  }

  fn hide_cursor_until_mouse_moves(&self) {
    self.connection.borrow().hide_cursor_until_mouse_moves()
  }

  fn is_cursor_visible(&self) -> bool {
    self.connection.borrow().is_cursor_visible()
  }

  fn should_auto_hide_scrollbars(&self) -> bool {
    self.with_common(|common| common.auto_hide_scrollbars)
  }

  fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
    let url = url.to_string();
    let username = username.to_string();
    let password = password.to_vec();
    self.background_executor().spawn(async move {
      let keyring = oo7::Keyring::new().await?;
      keyring.unlock().await?;
      keyring
        .create_item(
          KEYRING_LABEL,
          &vec![("url", &url), ("username", &username)],
          password,
          true,
        )
        .await?;
      Ok(())
    })
  }

  fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
    let url = url.to_string();
    self.background_executor().spawn(async move {
      let keyring = oo7::Keyring::new().await?;
      keyring.unlock().await?;

      let items = keyring.search_items(&vec![("url", &url)]).await?;

      for item in items {
        if item.label().await.is_ok_and(|label| label == KEYRING_LABEL) {
          let attributes = item.attributes().await?;
          let username = attributes
            .get("username")
            .context("Cannot find username in stored credentials")?;
          item.unlock().await?;
          let secret = item.secret().await?;

          // we lose the zeroizing capabilities at this boundary,
          // a current limitation GPUI's credentials api
          return Ok(Some((username.to_string(), secret.to_vec())));
        }
      }
      Ok(None)
    })
  }

  fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
    let url = url.to_string();
    self.background_executor().spawn(async move {
      let keyring = oo7::Keyring::new().await?;
      keyring.unlock().await?;

      let items = keyring.search_items(&vec![("url", &url)]).await?;

      for item in items {
        if item.label().await.is_ok_and(|label| label == KEYRING_LABEL) {
          item.delete().await?;
          return Ok(());
        }
      }

      Ok(())
    })
  }

  fn window_appearance(&self) -> WindowAppearance {
    self.with_common(|common| common.appearance)
  }

  fn button_layout(&self) -> Option<WindowButtonLayout> {
    Some(self.with_common(|common| common.button_layout))
  }

  fn register_url_scheme(&self, _: &str) -> Task<anyhow::Result<()>> {
    Task::ready(Err(anyhow!("register_url_scheme unimplemented")))
  }

  fn write_to_primary(&self, item: ClipboardItem) {
    self.connection.borrow().write_to_primary(item)
  }

  fn write_to_clipboard(&self, item: ClipboardItem) {
    self.connection.borrow().write_to_clipboard(item)
  }

  fn read_from_primary(&self) -> Option<ClipboardItem> {
    self.connection.borrow().read_from_primary()
  }

  fn read_from_clipboard(&self) -> Option<ClipboardItem> {
    self.connection.borrow().read_from_clipboard()
  }

  fn add_recent_document(&self, _path: &Path) {}
}

fn portal_error(err: ashpd::Error) -> anyhow::Error {
  match err {
    ashpd::Error::PortalNotFound(_) => anyhow!(FILE_PICKER_PORTAL_MISSING),
    err => err.into(),
  }
}

/// Opens `target` with the first of the desktop's launchers that succeeds. Returns whether one
/// did.
async fn run_open_commands(
  target: &std::ffi::OsStr,
  activation_token: Option<&str>,
  environment: &GraphicalEnvironment,
) -> bool {
  for mut command in open::commands(target) {
    environment.apply_to(&mut command);
    if let Some(token) = activation_token {
      command.env("XDG_ACTIVATION_TOKEN", token);
    }
    let program = format!("{:?}", command.get_program());
    match smol::process::Command::from(command).spawn() {
      Ok(mut child) => match child.status().await {
        Ok(status) if status.success() => return true,
        Ok(status) => log::error!("Command {} exited with status: {}", program, status),
        Err(error) => log::error!("Failed to get status from {}: {}", program, error),
      },
      Err(error) => log::error!("Failed to open with {}: {}", program, error),
    }
  }
  false
}

pub(super) fn open_uri_internal(
  executor: BackgroundExecutor,
  uri: &str,
  activation_token: Option<String>,
  environment: GraphicalEnvironment,
) {
  let Some(uri) = ashpd::Uri::parse(uri).log_err() else {
    return;
  };
  executor
    .spawn(async move {
      let opened = run_open_commands(
        uri.as_str().as_ref(),
        activation_token.as_deref(),
        &environment,
      )
      .await;
      if opened {
        return;
      }
      match ashpd::desktop::open_uri::OpenFileRequest::default()
        .activation_token(activation_token.map(ashpd::ActivationToken::from))
        .send_uri(&uri)
        .await
        .and_then(|e| e.response())
      {
        Ok(()) => {}
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {}
        Err(e) => log::error!("Failed to open with dbus: {}", e),
      }
    })
    .detach();
}

pub(super) fn reveal_path_internal(
  executor: BackgroundExecutor,
  path: PathBuf,
  activation_token: Option<String>,
  environment: GraphicalEnvironment,
) {
  executor
    .spawn(async move {
      let Some(dir) = File::open(&path).log_err() else {
        return;
      };
      match ashpd::desktop::open_uri::OpenDirectoryRequest::default()
        .activation_token(activation_token.clone().map(ashpd::ActivationToken::from))
        .send(&dir.as_fd())
        .await
      {
        Ok(_) => return,
        Err(e) => log::error!("Failed to open with dbus: {}", e),
      }
      let directory = if path.is_dir() {
        path.as_path()
      } else {
        path.parent().unwrap_or(Path::new(""))
      };
      run_open_commands(
        directory.as_os_str(),
        activation_token.as_deref(),
        &environment,
      )
      .await;
    })
    .detach();
}

pub(super) fn is_within_click_distance(a: Point<Pixels>, b: Point<Pixels>) -> bool {
  let diff = a - b;
  diff.x.abs() <= DOUBLE_CLICK_DISTANCE && diff.y.abs() <= DOUBLE_CLICK_DISTANCE
}

/// Creates an XKB context for keymaps supplied by the compositor.
///
/// Server keymaps are already resolved and need no local keyboard definitions.
/// Loading default include paths can fail on systems without those files.
pub(super) fn new_xkb_context() -> anyhow::Result<xkb::Context> {
  let context = xkb::Context::new(xkb::CONTEXT_NO_DEFAULT_INCLUDES);
  ensure!(
    !context.get_raw_ptr().is_null(),
    "libxkbcommon failed to create an XKB context"
  );
  Ok(context)
}

pub(super) fn get_xkb_compose_state(cx: &xkb::Context) -> Option<xkb::compose::State> {
  let mut locales = Vec::new();
  if let Some(locale) = env::var_os("LC_CTYPE") {
    locales.push(locale);
  }
  locales.push(OsString::from("C"));
  locales.into_iter().find_map(|locale| {
    xkb::compose::Table::new_from_locale(cx, &locale, xkb::compose::COMPILE_NO_FLAGS)
      .ok()
      .map(|table| xkb::compose::State::new(&table, xkb::compose::STATE_NO_FLAGS))
  })
}

pub(super) const PIPE_READ_TIMEOUT: Duration = Duration::from_secs(4);

pub(super) fn read_fd_with_timeout(
  mut fd: filedescriptor::FileDescriptor,
  timeout: Duration,
) -> Result<Vec<u8>> {
  fd.set_non_blocking(true)?;
  let mut buffer = Vec::new();
  let mut chunk = [0u8; 8192];
  loop {
    let mut poll_fds = [filedescriptor::pollfd {
      fd: fd.as_raw_fd(),
      events: filedescriptor::POLLIN,
      revents: 0,
    }];
    let ready = match filedescriptor::poll(&mut poll_fds, Some(timeout)) {
      Ok(ready) => ready,
      Err(filedescriptor::Error::Poll(err)) if err.kind() == std::io::ErrorKind::Interrupted => {
        continue;
      }
      Err(err) => return Err(err.into()),
    };
    if ready == 0 {
      anyhow::bail!("timed out waiting for data on pipe after {timeout:?}");
    }
    match fd.read(&mut chunk) {
      Ok(0) => return Ok(buffer),
      Ok(len) => buffer.extend_from_slice(&chunk[..len]),
      Err(err)
        if err.kind() == std::io::ErrorKind::WouldBlock
          || err.kind() == std::io::ErrorKind::Interrupted => {}
      Err(err) => return Err(err.into()),
    }
  }
}

pub(super) const DEFAULT_CURSOR_ICON_NAME: &str = "left_ptr";

pub(super) fn cursor_style_to_icon_names(style: CursorStyle) -> &'static [&'static str] {
  // Based on cursor names from chromium:
  // https://github.com/chromium/chromium/blob/d3069cf9c973dc3627fa75f64085c6a86c8f41bf/ui/base/cursor/cursor_factory.cc#L113
  match style {
    CursorStyle::Arrow => &[DEFAULT_CURSOR_ICON_NAME],
    CursorStyle::IBeam => &["text", "xterm"],
    CursorStyle::Crosshair => &["crosshair", "cross"],
    CursorStyle::ClosedHand => &["closedhand", "grabbing", "hand2"],
    CursorStyle::OpenHand => &["openhand", "grab", "hand1"],
    CursorStyle::PointingHand => &["pointer", "hand", "hand2"],
    CursorStyle::ResizeLeft => &["w-resize", "left_side"],
    CursorStyle::ResizeRight => &["e-resize", "right_side"],
    CursorStyle::ResizeLeftRight => &["ew-resize", "sb_h_double_arrow"],
    CursorStyle::ResizeUp => &["n-resize", "top_side"],
    CursorStyle::ResizeDown => &["s-resize", "bottom_side"],
    CursorStyle::ResizeUpDown => &["sb_v_double_arrow", "ns-resize"],
    CursorStyle::ResizeUpLeftDownRight => &["size_fdiag", "bd_double_arrow", "nwse-resize"],
    CursorStyle::ResizeUpRightDownLeft => &["size_bdiag", "nesw-resize", "fd_double_arrow"],
    CursorStyle::ResizeColumn => &["col-resize", "sb_h_double_arrow"],
    CursorStyle::ResizeRow => &["row-resize", "sb_v_double_arrow"],
    CursorStyle::IBeamCursorForVerticalLayout => &["vertical-text"],
    CursorStyle::OperationNotAllowed => &["not-allowed", "crossed_circle"],
    CursorStyle::DragLink => &["alias"],
    CursorStyle::DragCopy => &["copy"],
    CursorStyle::ContextualMenu => &["context-menu"],
  }
}

pub(super) fn log_cursor_icon_warning(message: impl std::fmt::Display) {
  if let Ok(xcursor_path) = env::var("XCURSOR_PATH") {
    log::warn!(
      "{:#}\ncursor icon loading may be failing if XCURSOR_PATH environment variable is invalid. \
                    XCURSOR_PATH overrides the default icon search. Its current value is '{}'",
      message,
      xcursor_path
    );
  } else {
    log::warn!("{:#}", message);
  }
}

fn guess_ascii(keycode: Keycode, shift: bool) -> Option<char> {
  let c = match (keycode.raw(), shift) {
    (24, _) => 'q',
    (25, _) => 'w',
    (26, _) => 'e',
    (27, _) => 'r',
    (28, _) => 't',
    (29, _) => 'y',
    (30, _) => 'u',
    (31, _) => 'i',
    (32, _) => 'o',
    (33, _) => 'p',
    (34, false) => '[',
    (34, true) => '{',
    (35, false) => ']',
    (35, true) => '}',
    (38, _) => 'a',
    (39, _) => 's',
    (40, _) => 'd',
    (41, _) => 'f',
    (42, _) => 'g',
    (43, _) => 'h',
    (44, _) => 'j',
    (45, _) => 'k',
    (46, _) => 'l',
    (47, false) => ';',
    (47, true) => ':',
    (48, false) => '\'',
    (48, true) => '"',
    (49, false) => '`',
    (49, true) => '~',
    (51, false) => '\\',
    (51, true) => '|',
    (52, _) => 'z',
    (53, _) => 'x',
    (54, _) => 'c',
    (55, _) => 'v',
    (56, _) => 'b',
    (57, _) => 'n',
    (58, _) => 'm',
    (59, false) => ',',
    (59, true) => '>',
    (60, false) => '.',
    (60, true) => '<',
    (61, false) => '/',
    (61, true) => '?',
    _ => return None,
  };

  Some(c)
}

pub(super) fn keystroke_from_xkb(
  state: &State,
  mut modifiers: gpui::Modifiers,
  keycode: Keycode,
) -> gpui::Keystroke {
  let key_utf32 = state.key_get_utf32(keycode);
  let key_utf8 = state.key_get_utf8(keycode);
  let key_sym = state.key_get_one_sym(keycode);

  let key = match key_sym {
    Keysym::Return => "enter".to_owned(),
    Keysym::Prior => "pageup".to_owned(),
    Keysym::Next => "pagedown".to_owned(),
    Keysym::ISO_Left_Tab => "tab".to_owned(),
    Keysym::KP_Prior => "pageup".to_owned(),
    Keysym::KP_Next => "pagedown".to_owned(),
    Keysym::XF86_Back => "back".to_owned(),
    Keysym::XF86_Forward => "forward".to_owned(),
    Keysym::XF86_Cut => "cut".to_owned(),
    Keysym::XF86_Copy => "copy".to_owned(),
    Keysym::XF86_Paste => "paste".to_owned(),
    Keysym::XF86_New => "new".to_owned(),
    Keysym::XF86_Open => "open".to_owned(),
    Keysym::XF86_Save => "save".to_owned(),

    Keysym::comma => ",".to_owned(),
    Keysym::period => ".".to_owned(),
    Keysym::less => "<".to_owned(),
    Keysym::greater => ">".to_owned(),
    Keysym::slash => "/".to_owned(),
    Keysym::question => "?".to_owned(),

    Keysym::semicolon => ";".to_owned(),
    Keysym::colon => ":".to_owned(),
    Keysym::apostrophe => "'".to_owned(),
    Keysym::quotedbl => "\"".to_owned(),

    Keysym::bracketleft => "[".to_owned(),
    Keysym::braceleft => "{".to_owned(),
    Keysym::bracketright => "]".to_owned(),
    Keysym::braceright => "}".to_owned(),
    Keysym::backslash => "\\".to_owned(),
    Keysym::bar => "|".to_owned(),

    Keysym::grave => "`".to_owned(),
    Keysym::asciitilde => "~".to_owned(),
    Keysym::exclam => "!".to_owned(),
    Keysym::at => "@".to_owned(),
    Keysym::numbersign => "#".to_owned(),
    Keysym::dollar => "$".to_owned(),
    Keysym::percent => "%".to_owned(),
    Keysym::asciicircum => "^".to_owned(),
    Keysym::ampersand => "&".to_owned(),
    Keysym::asterisk => "*".to_owned(),
    Keysym::parenleft => "(".to_owned(),
    Keysym::parenright => ")".to_owned(),
    Keysym::minus => "-".to_owned(),
    Keysym::underscore => "_".to_owned(),
    Keysym::equal => "=".to_owned(),
    Keysym::plus => "+".to_owned(),
    Keysym::space => "space".to_owned(),
    Keysym::BackSpace => "backspace".to_owned(),
    Keysym::Tab => "tab".to_owned(),
    Keysym::Delete => "delete".to_owned(),
    Keysym::Escape => "escape".to_owned(),

    Keysym::Left => "left".to_owned(),
    Keysym::Right => "right".to_owned(),
    Keysym::Up => "up".to_owned(),
    Keysym::Down => "down".to_owned(),
    Keysym::Home => "home".to_owned(),
    Keysym::End => "end".to_owned(),
    Keysym::Insert => "insert".to_owned(),

    _ => {
      let name = xkb::keysym_get_name(key_sym).to_lowercase();
      if key_sym.is_keypad_key() {
        name.replace("kp_", "")
      } else if let Some(key) = key_utf8.chars().next()
        && key_utf8.len() == 1
        && key.is_ascii()
      {
        if key.is_ascii_graphic() {
          key_utf8.to_lowercase()
        // map ctrl-a to `a`
        // ctrl-0..9 may emit control codes like ctrl-[, but
        // we don't want to map them to `[`
        } else if key_utf32 <= 0x1f && !name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
          ((key_utf32 as u8 + 0x40) as char)
            .to_ascii_lowercase()
            .to_string()
        } else {
          name
        }
      } else if let Some(key_en) = guess_ascii(keycode, modifiers.shift) {
        String::from(key_en)
      } else {
        name
      }
    }
  };

  // we only include the shift for upper-case letters by convention,
  // so don't include for numbers and symbols, but do include for
  // tab/enter, etc.
  if modifiers.shift && key.chars().count() == 1 && key.to_lowercase() == key.to_uppercase() {
    modifiers.shift = false;
  }

  // Ignore control characters (and DEL) for the purposes of key_char
  let key_char = (key_utf32 >= 32 && key_utf32 != 127 && !key_utf8.is_empty()).then_some(key_utf8);

  gpui::Keystroke {
    modifiers,
    key,
    key_char,
  }
}

/// Returns which symbol the dead key represents.
///
/// <https://developer.mozilla.org/en-US/docs/Web/API/UI_Events/Keyboard_event_key_values#dead_keycodes_for_linux>
pub fn keystroke_underlying_dead_key(keysym: Keysym) -> Option<String> {
  let symbol = match keysym {
    Keysym::dead_grave => "`",
    Keysym::dead_acute => "´",
    Keysym::dead_circumflex => "^",
    Keysym::dead_tilde => "~",
    Keysym::dead_macron => "¯",
    Keysym::dead_breve => "˘",
    Keysym::dead_abovedot => "˙",
    Keysym::dead_diaeresis => "¨",
    Keysym::dead_abovering => "˚",
    Keysym::dead_doubleacute => "˝",
    Keysym::dead_caron => "ˇ",
    Keysym::dead_cedilla => "¸",
    Keysym::dead_ogonek => "˛",
    Keysym::dead_iota => "ͅ",
    Keysym::dead_voiced_sound => "゙",
    Keysym::dead_semivoiced_sound => "゚",
    Keysym::dead_belowdot => "̣̣",
    Keysym::dead_hook => "̡",
    Keysym::dead_horn => "̛",
    Keysym::dead_stroke => "̶̶",
    Keysym::dead_abovecomma => "̓̓",
    Keysym::dead_abovereversedcomma => "ʽ",
    Keysym::dead_doublegrave => "̏",
    Keysym::dead_belowring => "˳",
    Keysym::dead_belowmacron => "̱",
    Keysym::dead_belowcircumflex => "ꞈ",
    Keysym::dead_belowtilde => "̰",
    Keysym::dead_belowbreve => "̮",
    Keysym::dead_belowdiaeresis => "̤",
    Keysym::dead_invertedbreve => "̯",
    Keysym::dead_belowcomma => "̦",
    Keysym::dead_small_schwa => "ə",
    Keysym::dead_capital_schwa => "Ə",
    _ => return None,
  };
  Some(symbol.to_owned())
}

pub(super) fn modifiers_from_xkb(keymap_state: &State) -> gpui::Modifiers {
  let is_active = |name| keymap_state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
  gpui::Modifiers {
    shift: is_active(xkb::MOD_NAME_SHIFT),
    alt: is_active(xkb::MOD_NAME_ALT),
    control: is_active(xkb::MOD_NAME_CTRL),
    platform: is_active(xkb::MOD_NAME_LOGO),
    function: false,
  }
}

pub(super) fn capslock_from_xkb(keymap_state: &State) -> gpui::Capslock {
  let on = keymap_state.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE);
  gpui::Capslock { on }
}

/// Resolve a Linux `dev_t` to PCI vendor/device IDs via sysfs, returning a
/// [`CompositorGpuHint`] that the GPU adapter selection code can use to
/// prioritize the compositor's rendering device.
pub(super) fn compositor_gpu_hint_from_dev_t(dev: u64) -> Option<gpui_wgpu::CompositorGpuHint> {
  fn dev_major(dev: u64) -> u32 {
    ((dev >> 8) & 0xfff) as u32 | (((dev >> 32) & !0xfff) as u32)
  }

  fn dev_minor(dev: u64) -> u32 {
    (dev & 0xff) as u32 | (((dev >> 12) & !0xff) as u32)
  }

  fn read_sysfs_hex_id(path: &str) -> Option<u32> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    u32::from_str_radix(trimmed.strip_prefix("0x").unwrap_or(trimmed), 16).ok()
  }

  let major = dev_major(dev);
  let minor = dev_minor(dev);

  let vendor_id = read_sysfs_hex_id(&format!("/sys/dev/char/{major}:{minor}/device/vendor"))?;
  let device_id = read_sysfs_hex_id(&format!("/sys/dev/char/{major}:{minor}/device/device"))?;

  log::info!(
    "Compositor GPU hint: vendor={:#06x}, device={:#06x} (from dev {major}:{minor})",
    vendor_id,
    device_id,
  );

  Some(gpui_wgpu::CompositorGpuHint {
    vendor_id,
    device_id,
  })
}

async fn inhibit_idle_sleep(reason: String, executor: BackgroundExecutor) -> Result<ActivityGuard> {
  let proxy = InhibitProxy::new()
    .await
    .context("Idle sleep prevention portal is unavailable")?;
  let request = proxy
    .inhibit(
      None,
      BitFlags::from(InhibitFlags::Suspend),
      InhibitOptions::default().set_reason(reason.as_str()),
    )
    .await
    .context("Failed to request idle sleep prevention")?;
  request
    .response()
    .context("Idle sleep prevention request was rejected")?;
  Ok(release_on_drop(request, executor))
}

fn release_on_drop(request: Request<()>, executor: BackgroundExecutor) -> ActivityGuard {
  ActivityGuard::new(move || {
    executor
      .spawn(async move {
        request
          .close()
          .await
          .context("Failed to release idle sleep prevention")
          .log_err();
      })
      .detach();
  })
}

async fn await_idle_sleep_prevention(
  guard_rx: oneshot::Receiver<Result<ActivityGuard>>,
  executor: &BackgroundExecutor,
) -> Result<ActivityGuard> {
  match futures::future::select(guard_rx, executor.timer(Duration::from_secs(10))).await {
    futures::future::Either::Left((Ok(result), _)) => result,
    futures::future::Either::Left((Err(_), _)) => {
      Err(anyhow!("Idle sleep prevention request was abandoned"))
    }
    futures::future::Either::Right(_) => Err(anyhow!(
      "Idle sleep prevention acquisition timed out after 10 seconds"
    )),
  }
}
