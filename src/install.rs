//! `install` / `uninstall` (D20, ticket 08 C2). Local verbs: they work without a daemon.
//! `~/.aerospace.toml` is edited format-preserving (toml_edit) or not at all; install
//! converges, backs up only when it changes something, and deletes nothing.

use crate::aero::{Aero, CliAero};
use crate::{config, ipc, paths};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use toml_edit::{Array, DocumentMut, value};

pub struct Edit {
    pub new_text: String,
    pub changes: Vec<String>,
    /// `persistent-workspaces` names this edit added (recorded for uninstall, P-8).
    pub added: Vec<String>,
}

/// Any `after-startup-command` entry that runs `aerospace-responsive ... reconcile` (D22).
pub fn is_poke(entry: &str) -> bool {
    entry.contains("aerospace-responsive") && entry.trim_end().ends_with("reconcile")
}

pub fn edit_aerospace_config(text: &str, managed: &BTreeSet<String>, poke: &str) -> Result<Edit> {
    let mut doc: DocumentMut = text.parse().context("~/.aerospace.toml is not valid TOML")?;
    let round_trips = doc.to_string() == text;
    let (mut changes, mut added) = (vec![], vec![]);

    // Load-bearing for placement convergence (ticket 03): persistent workspaces are never
    // invented as stubs for a vacated monitor.
    let persistent = doc
        .entry("persistent-workspaces")
        .or_insert(value(Array::new()))
        .as_array_mut()
        .context("persistent-workspaces is not an array")?;
    let have: BTreeSet<String> = persistent.iter().filter_map(|v| v.as_str().map(String::from)).collect();
    for w in managed.iter().filter(|w| !have.contains(*w)) {
        persistent.push(w.as_str());
        added.push(w.clone());
        changes.push(format!("persistent-workspaces += {w:?}"));
    }

    // Force-assignment beats move-workspace-to-monitor (ticket 08 F3); the tool owns placement (D15).
    if doc.remove("workspace-to-monitor-force-assignment").is_some() {
        changes.push("removed [workspace-to-monitor-force-assignment]".into());
    }

    let startup = doc
        .entry("after-startup-command")
        .or_insert(value(Array::new()))
        .as_array_mut()
        .context("after-startup-command is not an array")?;
    if !startup.iter().any(|v| v.as_str().is_some_and(is_poke)) {
        startup.push(poke);
        changes.push(format!("after-startup-command += {poke:?}"));
    }

    if changes.is_empty() {
        return Ok(Edit { new_text: text.to_string(), changes, added });
    }
    if !round_trips {
        bail!(
            "~/.aerospace.toml does not round-trip through the format-preserving editor; nothing was written.\n\
             Apply these changes by hand:\n  - {}",
            changes.join("\n  - ")
        );
    }
    Ok(Edit { new_text: doc.to_string(), changes, added })
}

/// Reverse of `edit_aerospace_config` (P-8): drop the names install added and the poke.
/// The force-assignment table is not restored (ticket 08 C4).
pub fn unedit_aerospace_config(text: &str, added: &[String], poke: Option<&str>) -> Result<Edit> {
    let mut doc: DocumentMut = text.parse().context("~/.aerospace.toml is not valid TOML")?;
    let round_trips = doc.to_string() == text;
    let mut changes = vec![];
    if let Some(arr) = doc.get_mut("persistent-workspaces").and_then(|i| i.as_array_mut()) {
        let before = arr.len();
        arr.retain(|v| !v.as_str().is_some_and(|s| added.iter().any(|a| a == s)));
        if arr.len() != before {
            changes.push(format!("persistent-workspaces -= {added:?}"));
        }
    }
    if let Some(arr) = doc.get_mut("after-startup-command").and_then(|i| i.as_array_mut()) {
        let before = arr.len();
        arr.retain(|v| !v.as_str().is_some_and(|s| poke.map_or_else(|| is_poke(s), |p| s == p)));
        if arr.len() != before {
            changes.push("after-startup-command -= the aerospace-responsive poke".into());
        }
    }
    if changes.is_empty() {
        return Ok(Edit { new_text: text.to_string(), changes, added: vec![] });
    }
    if !round_trips {
        bail!(
            "~/.aerospace.toml does not round-trip through the format-preserving editor; nothing was written.\n\
             Undo these by hand:\n  - {}",
            changes.join("\n  - ")
        );
    }
    Ok(Edit { new_text: doc.to_string(), changes, added: vec![] })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Step {
    Config,
    Launchd,
    Reload,
    Reconcile,
}

#[derive(Debug, clap::Args)]
pub struct Opts {
    /// Apply every selected step without asking
    #[arg(long)]
    pub yes: bool,
    /// Run only these steps (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<Step>,
    /// Skip these steps (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub skip: Vec<Step>,
}

pub fn selected(opts: &Opts, all: &[Step]) -> Vec<Step> {
    all.iter().copied().filter(|s| (opts.only.is_empty() || opts.only.contains(s)) && !opts.skip.contains(s)).collect()
}

/// What install changed, so uninstall can reverse exactly that (P-8).
#[derive(Debug, Default, Serialize, Deserialize)]
struct InstallRecord {
    #[serde(default)]
    added_persistent: Vec<String>,
    /// The exact `after-startup-command` entry install added, so uninstall removes that line
    /// and never a user-authored one that merely looks similar (P-8).
    #[serde(default)]
    poke: Option<String>,
    #[serde(default)]
    backups: Vec<PathBuf>,
}

impl InstallRecord {
    fn path() -> PathBuf {
        paths::state_dir().join("install.json")
    }
    fn load() -> Self {
        std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(paths::state_dir())?;
        std::fs::write(Self::path(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn install(opts: &Opts) -> Result<i32> {
    for step in selected(opts, &[Step::Config, Step::Launchd, Step::Reload, Step::Reconcile]) {
        match step {
            Step::Config => config_step(opts)?,
            Step::Launchd => launchd_step(opts)?,
            Step::Reload => {
                // Verification is a bonus: without a readable tool config there is simply
                // nothing to verify, which must not fail a run that skipped the config step.
                let managed = config::load(&paths::config_file()).ok().map(|l| l.config.managed_workspaces());
                reload_step(opts, managed.as_ref())?
            }
            Step::Reconcile => reconcile_step(opts)?,
        }
    }
    Ok(0)
}

pub fn uninstall(opts: &Opts) -> Result<i32> {
    for step in selected(opts, &[Step::Launchd, Step::Config, Step::Reload]) {
        match step {
            Step::Launchd => unload_step(opts)?,
            Step::Config => unconfig_step(opts)?,
            Step::Reload => reload_step(opts, None)?,
            Step::Reconcile => {}
        }
    }
    Ok(0)
}

fn confirm(opts: &Opts, question: &str, detail: &str) -> Result<bool> {
    println!("{detail}");
    if opts.yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        println!("{question} — not a terminal; re-run with --yes to apply");
        return Ok(false);
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn stamp() -> String {
    humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string().replace(':', "")
}

fn exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.to_string_lossy().contains("/target/") {
        eprintln!("warning: running from a build directory ({}); install the binary first: cargo install --path .", exe.display());
    }
    Ok(exe)
}

fn poke_command(exe: &Path) -> String {
    let s = exe.to_string_lossy();
    if s.contains(' ') { format!("exec-and-forget '{s}' reconcile") } else { format!("exec-and-forget {s} reconcile") }
}

fn write_with_backup(path: &Path, old: &str, new: &str, question: &str, changes: &[String], opts: &Opts) -> Result<Option<PathBuf>> {
    let diff = similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .header(&path.display().to_string(), "proposed")
        .to_string();
    if !confirm(opts, question, &format!("config: {}\n{diff}", changes.join("; ")))? {
        println!("config: skipped");
        return Ok(None);
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let backup = path.with_file_name(format!(".{name}.bak-aerospace-responsive-{}", stamp()));
    std::fs::copy(path, &backup).with_context(|| format!("cannot back up {}", path.display()))?;
    replace_file(path, new)?;
    println!("config: written (backup {})", backup.display());
    Ok(Some(backup))
}

/// Replace a file's contents atomically: write a sibling temp file, carry the original's
/// permissions, then rename over it. Symlinks are resolved first, so a dotfiles symlink keeps
/// pointing at its target instead of being replaced by a regular file.
fn replace_file(path: &Path, new: &str) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = target.with_file_name(format!(
        ".{}.aerospace-responsive-tmp-{}",
        target.file_name().unwrap_or_default().to_string_lossy(),
        stamp()
    ));
    std::fs::write(&tmp, new)?;
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot replace {}", target.display()));
    }
    Ok(())
}

fn config_step(opts: &Opts) -> Result<()> {
    let tool = config::load(&paths::config_file()).context("install needs the tool config first")?.config;
    let path = paths::aerospace_config().map_err(anyhow::Error::msg)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let poke = poke_command(&exe()?);
    let edit = edit_aerospace_config(&text, &tool.managed_workspaces(), &poke)?;
    if edit.changes.is_empty() {
        println!("config: nothing to do");
        return Ok(());
    }
    if let Some(backup) = write_with_backup(&path, &text, &edit.new_text, "config: apply these changes?", &edit.changes, opts)? {
        let mut rec = InstallRecord::load();
        for w in edit.added {
            if !rec.added_persistent.contains(&w) {
                rec.added_persistent.push(w);
            }
        }
        if edit.changes.iter().any(|c| c.starts_with("after-startup-command +=")) {
            rec.poke = Some(poke);
        }
        rec.backups.push(backup);
        rec.save()?;
    }
    Ok(())
}

fn unconfig_step(opts: &Opts) -> Result<()> {
    let mut rec = InstallRecord::load();
    let path = paths::aerospace_config().map_err(anyhow::Error::msg)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let edit = unedit_aerospace_config(&text, &rec.added_persistent, rec.poke.as_deref())?;
    if edit.changes.is_empty() {
        println!("config: nothing to undo");
        return Ok(());
    }
    if let Some(backup) = write_with_backup(&path, &text, &edit.new_text, "config: undo these changes?", &edit.changes, opts)? {
        rec.added_persistent.clear();
        rec.poke = None;
        rec.backups.push(backup);
        rec.save()?;
    }
    Ok(())
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn service() -> String {
    format!("{}/{}", domain(), paths::LABEL)
}

fn launchctl(args: &[&str]) -> Result<Output> {
    Command::new("launchctl").args(args).output().context("cannot run launchctl")
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|c| c.is_file())
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Ticket 10 P4 / ticket 08 note: plain binary, RunAtLoad, KeepAlive, ThrottleInterval 10,
/// logs under ~/Library/Logs/aerospace-responsive/. PATH carries the `aerospace` CLI's dir.
pub fn plist_text(exe: &Path, aerospace_dir: &Path, log_dir: &Path) -> String {
    let log = xml(&log_dir.join("daemon.log").to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>daemon</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = paths::LABEL,
        exe = xml(&exe.to_string_lossy()),
        path = xml(&aerospace_dir.to_string_lossy()),
    )
}

fn launchd_step(opts: &Opts) -> Result<()> {
    let aerospace = which("aerospace").context("`aerospace` is not on PATH")?;
    let plist = plist_text(&exe()?, aerospace.parent().expect("a file has a parent"), &paths::log_dir());
    let path = paths::launch_agent();
    let loaded = launchctl(&["print", &service()])?.status.success();
    if loaded && std::fs::read_to_string(&path).ok().as_deref() == Some(plist.as_str()) {
        println!("launchd: nothing to do");
        return Ok(());
    }
    if !confirm(opts, "launchd: write and load this LaunchAgent?", &format!("launchd: {}\n{plist}", path.display()))? {
        println!("launchd: skipped");
        return Ok(());
    }
    std::fs::create_dir_all(paths::log_dir())?;
    std::fs::create_dir_all(path.parent().expect("LaunchAgents dir"))?;
    std::fs::write(&path, &plist)?;
    if loaded {
        launchctl(&["bootout", &service()])?;
    }
    let out = launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {} (if it was just unloaded, run `install --only launchd` again)",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("launchd: loaded {} (log: {})", paths::LABEL, paths::log_dir().join("daemon.log").display());
    Ok(())
}

fn unload_step(opts: &Opts) -> Result<()> {
    let path = paths::launch_agent();
    let loaded = launchctl(&["print", &service()])?.status.success();
    if !loaded && !path.exists() {
        println!("launchd: nothing to undo");
        return Ok(());
    }
    if !confirm(opts, "launchd: unload and remove the LaunchAgent?", &format!("launchd: {}", path.display()))? {
        println!("launchd: skipped");
        return Ok(());
    }
    if loaded {
        launchctl(&["bootout", &service()])?;
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    println!("launchd: unloaded");
    Ok(())
}

fn placement() -> Result<BTreeMap<String, u32>> {
    Ok(CliAero.workspaces()?.into_iter().map(|r| (r.workspace, r.monitor)).collect())
}

/// Ticket 08 C7 step 1: reload moves nothing (F3); verify the derived names are listed.
fn reload_step(opts: &Opts, expect: Option<&BTreeSet<String>>) -> Result<()> {
    if !confirm(opts, "reload: run `aerospace reload-config --no-gui`?", "reload: AeroSpace re-reads ~/.aerospace.toml")? {
        println!("reload: skipped");
        return Ok(());
    }
    let before = placement()?;
    let out = Command::new("aerospace").args(["reload-config", "--no-gui"]).output().context("cannot run aerospace")?;
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).trim().to_string();
    if !out.status.success() || !said.is_empty() {
        bail!("reload-config reported: {said}");
    }
    let after = placement()?;
    let moved: Vec<String> = after
        .iter()
        .filter(|(w, m)| before.get(*w).is_some_and(|b| b != *m))
        .map(|(w, m)| format!("{w} -> monitor {m}"))
        .collect();
    if moved.is_empty() {
        println!("reload: ok, nothing moved");
    } else {
        println!("reload: ok, but these moved: {}", moved.join(", "));
    }
    if let Some(names) = expect {
        let missing: Vec<&String> = names.iter().filter(|n| !after.contains_key(*n)).collect();
        if !missing.is_empty() {
            bail!("not listed by list-workspaces after reload: {missing:?}");
        }
        println!("reload: persistent workspaces present: {}", names.iter().cloned().collect::<Vec<_>>().join(" "));
    }
    Ok(())
}

fn reconcile_step(opts: &Opts) -> Result<()> {
    if !confirm(opts, "reconcile: run a first pass now?", "reconcile: the daemon runs one pass (this moves workspaces)")? {
        println!("reconcile: skipped");
        return Ok(());
    }
    match ipc::call(&paths::daemon_socket(), &ipc::Request::Reconcile { dry_run: false, config: None }) {
        Ok(r) => {
            print!("reconcile: {}", r.text);
            if r.exit != 0 {
                bail!("the pass did not succeed; see `aerospace-responsive status`");
            }
            Ok(())
        }
        Err(ipc::CallError::NoDaemon) => bail!("daemon not running (run the launchd step first)"),
        Err(ipc::CallError::Io(e)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_LIKE: &str = r#"# my aerospace config
config-version = 2

# keep these around
persistent-workspaces = ["1", "2", "T", "U"]

after-startup-command = [
  'exec-and-forget ~/Projects/Scripts/macos/ice-cubes-watcher.sh',
  'exec-and-forget ~/Projects/Scripts/macos/aerospace-top-gap.sh',
]

[workspace-to-monitor-force-assignment]
T = ["LEN T2364pA"]

[gaps]
inner.horizontal = 8   # tight
"#;

    const POKE: &str = "exec-and-forget /Users/me/.cargo/bin/aerospace-responsive reconcile";

    fn managed() -> BTreeSet<String> {
        ["1", "2", "3", "C", "T"].iter().map(|s| s.to_string()).collect()
    }

    fn strings(doc: &str, key: &str) -> Vec<String> {
        let d: DocumentMut = doc.parse().unwrap();
        d[key].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn edit_adds_missing_names_removes_force_assignment_and_adds_the_poke() {
        let e = edit_aerospace_config(LIVE_LIKE, &managed(), POKE).unwrap();
        assert_eq!(strings(&e.new_text, "persistent-workspaces"), ["1", "2", "T", "U", "3", "C"]);
        assert_eq!(e.added, ["3", "C"]);
        assert!(!e.new_text.contains("workspace-to-monitor-force-assignment"));
        assert_eq!(strings(&e.new_text, "after-startup-command").last().unwrap(), POKE);
        assert!(e.new_text.contains("# keep these around"));
        assert!(e.new_text.contains("inner.horizontal = 8   # tight"));
        assert_eq!(e.changes.len(), 4);
    }

    #[test]
    fn edit_converges() {
        let once = edit_aerospace_config(LIVE_LIKE, &managed(), POKE).unwrap();
        let twice = edit_aerospace_config(&once.new_text, &managed(), POKE).unwrap();
        assert!(twice.changes.is_empty());
        assert_eq!(twice.new_text, once.new_text);
    }

    #[test]
    fn an_existing_poke_with_another_path_counts() {
        let text = "after-startup-command = ['exec-and-forget /opt/bin/aerospace-responsive reconcile']\n";
        let e = edit_aerospace_config(text, &BTreeSet::new(), POKE).unwrap();
        assert!(e.changes.is_empty());
    }

    #[test]
    fn missing_keys_are_created() {
        let e = edit_aerospace_config("config-version = 2\n", &managed(), POKE).unwrap();
        assert_eq!(strings(&e.new_text, "persistent-workspaces"), ["1", "2", "3", "C", "T"]);
        assert_eq!(strings(&e.new_text, "after-startup-command"), [POKE]);
    }

    #[test]
    fn non_array_persistent_workspaces_is_an_error() {
        assert!(edit_aerospace_config("persistent-workspaces = \"1\"\n", &managed(), POKE).is_err());
    }

    #[test]
    fn unedit_removes_exactly_what_install_added() {
        let installed = edit_aerospace_config(LIVE_LIKE, &managed(), POKE).unwrap();
        let back = unedit_aerospace_config(&installed.new_text, &installed.added, Some(POKE)).unwrap();
        assert_eq!(strings(&back.new_text, "persistent-workspaces"), ["1", "2", "T", "U"]);
        assert_eq!(strings(&back.new_text, "after-startup-command").len(), 2);
        assert!(!back.new_text.contains("force-assignment"), "C4: never restored");
        assert!(unedit_aerospace_config(&back.new_text, &installed.added, Some(POKE)).unwrap().changes.is_empty());
    }

    #[test]
    fn uninstall_removes_only_the_line_install_wrote() {
        let text = format!(
            "after-startup-command = [\n  'exec-and-forget /Users/me/bin/aerospace-responsive-wrapper.sh reconcile',\n  '{POKE}',\n]\n"
        );
        let back = unedit_aerospace_config(&text, &[], Some(POKE)).unwrap();
        let left = strings(&back.new_text, "after-startup-command");
        assert_eq!(left, ["exec-and-forget /Users/me/bin/aerospace-responsive-wrapper.sh reconcile"]);
    }

    #[test]
    fn replace_file_writes_through_a_symlink_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("link.toml");
        std::fs::write(&real, "old\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        replace_file(&link, "new\n").unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the symlink must survive");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new\n");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains("tmp")).collect();
        assert!(leftovers.is_empty(), "no temp file left behind");
    }

    #[test]
    fn steps_are_choosable() {
        let all = [Step::Config, Step::Launchd, Step::Reload, Step::Reconcile];
        let o = |only: Vec<Step>, skip: Vec<Step>| Opts { yes: true, only, skip };
        assert_eq!(selected(&o(vec![], vec![]), &all), all);
        assert_eq!(selected(&o(vec![Step::Config, Step::Reload], vec![]), &all), [Step::Config, Step::Reload]);
        assert_eq!(selected(&o(vec![], vec![Step::Reconcile]), &all), [Step::Config, Step::Launchd, Step::Reload]);
    }

    #[test]
    fn plist_matches_ticket_10_shape() {
        let p = plist_text(
            Path::new("/Users/me/bin/a&b/aerospace-responsive"),
            Path::new("/opt/homebrew/bin"),
            Path::new("/Users/me/Library/Logs/aerospace-responsive"),
        );
        assert!(p.contains("<string>/Users/me/bin/a&amp;b/aerospace-responsive</string>\n    <string>daemon</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(p.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(p.contains("<key>ThrottleInterval</key>\n  <integer>10</integer>"));
        assert!(p.contains("<string>/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>"));
        assert!(p.contains("daemon.log"));
    }
}
