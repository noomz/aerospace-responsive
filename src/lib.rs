//! aerospace-responsive: reshape AeroSpace workspace placement and root layout when the
//! attached display set changes. Design record: .scratch/responsive-layout/map.md (D1-D26).

pub mod aero;
pub mod config;
pub mod display;
pub mod macos;
pub mod pass;
pub mod paths;
pub mod restore;
pub mod rules;
pub mod state;
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
