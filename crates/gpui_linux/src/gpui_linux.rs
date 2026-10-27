#![cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux;

pub use linux::{background_executor, current_platform, linux_platform};
