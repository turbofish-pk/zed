pub mod async_net;
#[cfg(target_os = "windows")]
pub mod listener;
#[cfg(target_os = "windows")]
pub mod socket;
#[cfg(target_os = "windows")]
pub mod stream;
#[cfg(target_os = "windows")]
mod util;

#[cfg(target_os = "windows")]
pub use listener::*;
#[cfg(target_os = "windows")]
pub use socket::*;

pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(target_os = "windows")]
pub use stream::*;

