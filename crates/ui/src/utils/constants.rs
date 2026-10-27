use gpui::{Pixels, Window, px};

// Use pixels here instead of a rem-based size because the macOS traffic
// lights are a static size, and don't scale with the rest of the UI.
//
// Magic number: There is one extra pixel of padding on the left side due to
// the 1px border around the window on macOS apps.
pub const TRAFFIC_LIGHT_PADDING: f32 =  71. ;

/// Returns the platform-appropriate title bar height.
///
/// On Windows, this returns a fixed height of 32px.
/// On other platforms, it scales with the window's rem size (1.75x) with a minimum of 34px.

pub fn platform_title_bar_height(window: &Window) -> Pixels {
    (1.75 * window.rem_size()).max(px(34.))
}
