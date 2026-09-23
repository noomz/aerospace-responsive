//! Every filesystem location the tool uses (plan P-11).

use std::ffi::CStr;
use std::path::{Path, PathBuf};

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

/// Where AeroSpace looks for its config, in its own order
/// (ConfigFile.swift:4-14): the dotfile, then `$XDG_CONFIG_HOME/aerospace/aerospace.toml`.
pub fn aerospace_config_candidates() -> Vec<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"));
    vec![home().join(".aerospace.toml"), xdg.join("aerospace").join("aerospace.toml")]
}

/// The config AeroSpace is actually reading. AeroSpace itself refuses to start when both
/// candidates exist, so having two is an error here too rather than a guess.
pub fn aerospace_config() -> Result<PathBuf, String> {
    resolve_config(aerospace_config_candidates(), |p| p.exists())
}

fn resolve_config(candidates: Vec<PathBuf>, exists: impl Fn(&Path) -> bool) -> Result<PathBuf, String> {
    let found: Vec<PathBuf> = candidates.iter().filter(|p| exists(p)).cloned().collect();
    match found.len() {
        1 => Ok(found.into_iter().next().expect("exactly one")),
        0 => Err(format!(
            "no AeroSpace config found; looked for {}",
            candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" and ")
        )),
        _ => Err(format!(
            "AeroSpace has two configs and refuses to start with both: {}",
            found.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" and ")
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn resolves_the_single_existing_aerospace_config() {
        let c = paths(&["/home/me/.aerospace.toml", "/home/me/.config/aerospace/aerospace.toml"]);
        let only_xdg = resolve_config(c.clone(), |p| p.ends_with("aerospace/aerospace.toml")).unwrap();
        assert_eq!(only_xdg, PathBuf::from("/home/me/.config/aerospace/aerospace.toml"));
        let only_dotfile = resolve_config(c.clone(), |p| p.ends_with(".aerospace.toml")).unwrap();
        assert_eq!(only_dotfile, PathBuf::from("/home/me/.aerospace.toml"));
    }

    #[test]
    fn both_or_neither_is_an_error() {
        let c = paths(&["/home/me/.aerospace.toml", "/home/me/.config/aerospace/aerospace.toml"]);
        assert!(resolve_config(c.clone(), |_| true).unwrap_err().contains("two configs"));
        assert!(resolve_config(c, |_| false).unwrap_err().contains("no AeroSpace config found"));
    }
}
