//! aerospace-responsive: reshape AeroSpace workspace placement and root layout when the
//! attached display set changes. Comments cite design decisions as D1-D26.

pub mod aero;
pub mod config;
pub mod daemon;
pub mod display;
pub mod gaps;
pub mod install;
pub mod ipc;
pub mod macos;
pub mod pass;
pub mod paths;
pub mod render;
pub mod restore;
pub mod rules;
pub mod state;
pub mod tiling;
pub mod watch;
pub mod worker;
pub mod world;
#[cfg(test)]
pub mod testutil;


/// Timestamped line on stderr; launchd sends stderr to ~/Library/Logs/aerospace-responsive/.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", humantime::format_rfc3339_seconds(std::time::SystemTime::now()), format!($($arg)*))
    };
}
