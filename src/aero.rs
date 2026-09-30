//! The only door to AeroSpace: its CLI with `--json`. `Aero` is a trait so the pass can be
//! tested against a fake.

use crate::config::Axis;
use crate::display::Display;
use crate::world::{AeroMonitor, DETAIL_FORMAT, MONITOR_FORMAT, Snapshot, WINDOW_FORMAT, WORKSPACE_FORMAT, WinInfo, WorkspaceRow};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Cmd {
    /// `monitor` is the resolved id for bookkeeping; `target` is what AeroSpace receives (P-6).
    Move { ws: String, monitor: u32, target: String },
    Flatten { ws: String },
    Layout { ws: String, axis: Axis },
    Focus { ws: String },
    /// Exclusive tiling's else-action, and its undo (I.aero).
    Float { id: u32 },
    Tile { id: u32 },
}

impl Cmd {
    pub fn args(&self) -> Vec<String> {
        let s = |x: &str| x.to_string();
        match self {
            Cmd::Move { ws, target, .. } => vec![s("move-workspace-to-monitor"), s("--workspace"), ws.clone(), s("--"), target.clone()],
            Cmd::Flatten { ws } => vec![s("flatten-workspace-tree"), s("--workspace"), ws.clone()],
            // horizontal|vertical only, so an accordion root keeps being an accordion (D18).
            Cmd::Layout { ws, axis } => {
                let l = if *axis == Axis::H { "horizontal" } else { "vertical" };
                vec![s("layout"), s("--root"), s("--workspace"), ws.clone(), s(l)]
            }
            // `workspace` is the only non-moving way to show a workspace; it always focuses (13).
            Cmd::Focus { ws } => vec![s("workspace"), s("--"), ws.clone()],
            Cmd::Float { id } => vec![s("layout"), s("floating"), s("--window-id"), id.to_string()],
            Cmd::Tile { id } => vec![s("layout"), s("tiling"), s("--window-id"), id.to_string()],
        }
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "aerospace")?;
        for a in self.args() {
            if a.chars().any(|c| c.is_whitespace() || "'\"\\$()[]|*?".contains(c)) {
                write!(f, " '{}'", a.replace('\'', r"'\''"))?;
            } else {
                write!(f, " {a}")?;
            }
        }
        Ok(())
    }
}

/// `move-workspace-to-monitor` target (plan P-6): an anchored name regex when the name is
/// unique and not a positional " (n)" disambiguation, else the monitor-id of this pass.
pub fn monitor_target(ds: &[Display], id: u32) -> String {
    let Some(d) = ds.iter().find(|d| d.aero_id == id) else { return id.to_string() };
    // Some displays report no name at all (seen on a real HDMI panel), and `^$` is not a
    // monitor pattern worth betting a move on.
    if d.aero_name.trim().is_empty() {
        return id.to_string();
    }
    let unique = ds.iter().filter(|o| o.aero_name == d.aero_name).count() == 1;
    let suffixed = d
        .aero_name
        .strip_suffix(')')
        .and_then(|n| n.rsplit_once(" ("))
        .is_some_and(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
    if unique && !suffixed { format!("^{}$", regex_escape(&d.aero_name)) } else { id.to_string() }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub trait Aero {
    fn monitors(&mut self) -> Result<Vec<AeroMonitor>>;
    fn windows(&mut self) -> Result<Snapshot>;
    fn workspaces(&mut self) -> Result<Vec<WorkspaceRow>>;
    /// Every window with app, title and layout: exclusive tiling's read, kept out of the
    /// pass snapshot so display passes never pay for titles.
    fn window_details(&mut self) -> Result<Vec<WinInfo>>;
    /// `Err` carries AeroSpace's stderr, e.g. a force-assignment refusal.
    fn run(&mut self, cmd: &Cmd) -> Result<(), String>;
}

/// Spawns `aerospace` from PATH (the LaunchAgent sets PATH; see install.rs).
pub struct CliAero;

impl CliAero {
    fn json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let out = Command::new("aerospace").args(args).output().context("cannot spawn `aerospace` (is it on PATH?)")?;
        if !out.status.success() {
            bail!("aerospace {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        serde_json::from_slice(&out.stdout).with_context(|| format!("aerospace {}: unexpected output", args.join(" ")))
    }
}

impl Aero for CliAero {
    fn monitors(&mut self) -> Result<Vec<AeroMonitor>> {
        self.json(&["list-monitors", "--json", "--format", MONITOR_FORMAT])
    }
    fn windows(&mut self) -> Result<Snapshot> {
        Ok(Snapshot { windows: self.json(&["list-windows", "--all", "--json", "--format", WINDOW_FORMAT])? })
    }
    fn workspaces(&mut self) -> Result<Vec<WorkspaceRow>> {
        self.json(&["list-workspaces", "--all", "--json", "--format", WORKSPACE_FORMAT])
    }
    fn window_details(&mut self) -> Result<Vec<WinInfo>> {
        self.json(&["list-windows", "--all", "--json", "--format", DETAIL_FORMAT])
    }
    fn run(&mut self, cmd: &Cmd) -> Result<(), String> {
        let out = Command::new("aerospace").args(cmd.args()).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("exit {}", out.status) } else { err })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeAero, builtin, desk3, len_landscape};

    #[test]
    fn commands_render_as_copy_pasteable_lines() {
        let m = Cmd::Move { ws: "1".into(), monitor: 2, target: "^Built-in Retina Display$".into() };
        assert_eq!(m.to_string(), "aerospace move-workspace-to-monitor --workspace 1 -- '^Built-in Retina Display$'");
        assert_eq!(Cmd::Layout { ws: "1".into(), axis: Axis::V }.to_string(), "aerospace layout --root --workspace 1 vertical");
        assert_eq!(Cmd::Flatten { ws: "C".into() }.to_string(), "aerospace flatten-workspace-tree --workspace C");
        assert_eq!(Cmd::Focus { ws: "T".into() }.to_string(), "aerospace workspace -- T");
        assert_eq!(Cmd::Float { id: 5230 }.to_string(), "aerospace layout floating --window-id 5230");
        assert_eq!(Cmd::Tile { id: 4380 }.to_string(), "aerospace layout tiling --window-id 4380");
    }

    #[test]
    fn fake_float_and_tile_apply_by_window_id() {
        let mut f = FakeAero::new(&desk3(), &[("T", 2, true, 1, Axis::H)], "T");
        f.add_window("T", 7, "com.apple.Safari", "GitHub", false);
        f.run(&Cmd::Float { id: 7 }).unwrap();
        let d = f.window_details().unwrap();
        let w = d.iter().find(|w| w.id == 7).unwrap();
        assert!(w.floating() && !w.tiled());
        f.run(&Cmd::Tile { id: 7 }).unwrap();
        assert!(f.window_details().unwrap().iter().find(|w| w.id == 7).unwrap().tiled());
        assert!(f.run(&Cmd::Tile { id: 999 }).is_err());
    }

    #[test]
    fn monitor_target_prefers_a_unique_unsuffixed_name() {
        let ds = desk3();
        assert_eq!(monitor_target(&ds, 2), "^Built-in Retina Display$");
        assert_eq!(monitor_target(&ds, 1), "1"); // "LEN T23i-10 (2)" is positional
        let mut lone = len_landscape(2);
        lone.aero_name = "LEN T23i-10".into();
        assert_eq!(monitor_target(&[builtin(1), lone], 2), "^LEN T23i-10$");
        // A nameless display (real: an HDMI panel with no EDID name) must go by id.
        let mut nameless = len_landscape(3);
        nameless.aero_name = String::new();
        assert_eq!(monitor_target(&[builtin(1), nameless], 3), "3");
    }

    #[test]
    fn fake_move_changes_visibility_on_both_monitors() {
        let ds = desk3();
        let mut f = FakeAero::new(&ds, &[("1", 2, true, 1, Axis::H), ("T", 2, false, 1, Axis::H), ("2", 3, true, 1, Axis::H)], "1");
        f.run(&Cmd::Move { ws: "1".into(), monitor: 3, target: "3".into() }).unwrap();
        assert_eq!(f.ws("1").monitor, 3);
        assert!(f.ws("1").visible && !f.ws("2").visible);
        assert!(f.ws("T").visible, "source monitor gets a stub");
        f.refuse.insert("T".into());
        assert!(f.run(&Cmd::Move { ws: "T".into(), monitor: 1, target: "1".into() }).is_err());
    }
}
