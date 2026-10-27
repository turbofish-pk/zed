//! Convenience crate that re-exports GPUI's platform traits and the
//! `current_platform` constructor so consumers don't need `#[cfg]` gating.

pub use gpui::Platform;

use std::rc::Rc;

/// Returns a background executor for the current platform.
pub fn background_executor() -> gpui::BackgroundExecutor {
  #[cfg(all(
    any(target_os = "linux", target_os = "freebsd"),
    not(target_env = "ohos")
  ))]
  {
    gpui_linux::background_executor()
  }

  #[cfg(not(all(
    any(target_os = "linux", target_os = "freebsd"),
    not(target_env = "ohos")
  )))]
  {
    current_platform(true).background_executor()
  }
}

pub fn application() -> gpui::Application {
  #[cfg(target_family = "wasm")]
  {
    application_with_web_backend(gpui_web::WebBackendPreference::Auto)
  }

  #[cfg(not(target_family = "wasm"))]
  gpui::Application::with_platform(current_platform(false))
}

/// Returns a headless app.
///
/// # Panics
///
/// Panics on Linux, where this build has no headless mode.
pub fn headless() -> gpui::Application {
  gpui::Application::with_platform(current_platform(true))
}

/// Returns a Linux app connected to the Wayland compositor the process environment names.
///
/// `allowed_modes` is ignored: only Wayland is supported. Connect to another environment with
/// [`gpui::Application::with_windowing`].
#[cfg(all(
  any(target_os = "linux", target_os = "freebsd"),
  not(target_env = "ohos")
))]
pub fn linux(allowed_modes: gpui::WindowingModes) -> gpui::Application {
  gpui::Application::with_platform(gpui_linux::linux_platform(allowed_modes))
}

#[cfg(target_family = "wasm")]
pub use gpui_web::WebBackendPreference;

#[cfg(target_family = "wasm")]
pub fn application_with_web_backend(backend_preference: WebBackendPreference) -> gpui::Application {
  let platform = Rc::new(gpui_web::WebPlatform::new_with_backend(
    true,
    backend_preference,
  ));
  let http_client = std::sync::Arc::new(platform.fetch_http_client());
  gpui::Application::with_platform(platform).with_http_client(http_client)
}

/// Unlike `application`, this function returns a single-threaded web application.
#[cfg(target_family = "wasm")]
pub fn single_threaded_web() -> gpui::Application {
  let platform = Rc::new(gpui_web::WebPlatform::new(false));
  let http_client = std::sync::Arc::new(platform.fetch_http_client());
  gpui::Application::with_platform(platform).with_http_client(http_client)
}

/// Initializes panic hooks and logging for the web platform.
/// Call this before running the application in a wasm_bindgen entrypoint.
#[cfg(target_family = "wasm")]
pub fn web_init() {
  console_error_panic_hook::set_once();
  gpui_web::init_logging();
}

/// Returns the default [`Platform`] for the current OS.
pub fn current_platform(headless: bool) -> Rc<dyn Platform> {
  #[cfg(target_os = "ios")]
  {
    gpui_ios::current_platform(headless)
  }

  #[cfg(target_os = "macos")]
  {
    Rc::new(gpui_macos::MacPlatform::new(headless))
  }

  #[cfg(target_os = "windows")]
  {
    Rc::new(
      gpui_windows::WindowsPlatform::new(headless).expect("failed to initialize Windows platform"),
    )
  }

  #[cfg(target_env = "ohos")]
  {
    let _ = headless;
    panic!(
            "gpui_platform does not select an OpenHarmony backend; inject gpui-ohos with Application::with_platform"
        )
  }

  #[cfg(all(
    any(target_os = "linux", target_os = "freebsd"),
    not(target_env = "ohos")
  ))]
  {
    gpui_linux::current_platform(headless)
  }

  #[cfg(target_family = "wasm")]
  {
    let _ = headless;
    Rc::new(gpui_web::WebPlatform::new(true))
  }
}

/// Returns the current platform's text system for benchmark contexts, built once per
/// thread and shared by every context on it.
///
/// `#[gpui::bench]` creates a context for every Criterion routine call. Building a whole
/// platform each time only to take its text system leaks the platform's threads on Linux
/// (a 20-worker dispatcher and a timer thread per call), which slows every later
/// benchmark in the process.
///
/// Panics on Linux, where this build has no headless mode.
#[cfg(feature = "bench-support")]
pub fn bench_text_system() -> std::sync::Arc<dyn gpui::PlatformTextSystem> {
  thread_local! {
      static TEXT_SYSTEM: std::sync::Arc<dyn gpui::PlatformTextSystem> =
          current_platform(true).text_system();
  }
  TEXT_SYSTEM.with(|text_system| text_system.clone())
}

/// Returns a new [`HeadlessRenderer`] for the current platform, if available.
#[cfg(any(feature = "bench-support", feature = "test-support"))]
pub fn current_headless_renderer() -> anyhow::Result<Option<Box<dyn gpui::PlatformHeadlessRenderer>>>
{
  #[cfg(target_os = "macos")]
  {
    Ok(Some(Box::new(
      gpui_macos::metal_renderer::MetalHeadlessRenderer::new()?,
    )))
  }

  {
    gpui_wgpu::WgpuHeadlessRenderer::new()
      .map(|renderer| Some(Box::new(renderer) as Box<dyn gpui::PlatformHeadlessRenderer>))
  }

  #[cfg(not(any(target_os = "macos", target_os = "linux")))]
  {
    Ok(None)
  }
}
