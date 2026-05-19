#[macro_use]
extern crate log;

mod utils;

pub mod args;
pub mod live_only_sink;
pub mod live_sink;

#[cfg(target_os = "macos")]
pub mod cmd_record_mac;
#[cfg(target_os = "macos")]
pub mod cmd_setup_mac;
#[cfg(target_os = "linux")]
pub mod cmd_setup_linux;
