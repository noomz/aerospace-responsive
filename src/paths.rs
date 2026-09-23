//! Every filesystem location the tool uses (plan P-11).

use std::ffi::CStr;
use std::path::PathBuf;

pub const LABEL: &str = "com.noomz.aerospace-responsive";

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"))
}

/// The tool's own config (D12), in its own directory so the directory can be watched
/// for atomic-save editors that replace the file.
pub fn config_file() -> PathBuf {
    home().join(".config/aerospace-responsive/aerospace-responsive.toml")
}

pub fn state_dir() -> PathBuf {
    home().join("Library/Application Support/aerospace-responsive")
}

pub fn log_dir() -> PathBuf {
    home().join("Library/Logs/aerospace-responsive")
}

pub fn daemon_socket() -> PathBuf {
    PathBuf::from(format!("/tmp/aerospace-responsive-{}.sock", user_name()))
}

/// AeroSpace's server socket (Sources/Common/util/commonUtil.swift:5).
pub fn aerospace_socket() -> PathBuf {
    PathBuf::from(format!("/tmp/bobko.aerospace-{}.sock", user_name()))
}

pub fn aerospace_config() -> PathBuf {
    home().join(".aerospace.toml")
}

pub fn launch_agent() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

/// Login name from the password database; launchd agents do not always carry $USER.
pub fn user_name() -> String {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return std::env::var("USER").unwrap_or_else(|_| "unknown".into());
        }
        CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
    }
}
