//! Test fixtures mirroring the user's real desk.

use crate::display::{Display, Rect, Screen};

fn screen(cg_id: u32, name: &str, bounds: Rect, px: (u64, u64), rotation: f64, main: bool, builtin: bool) -> Screen {
    Screen {
        cg_id,
        name: name.into(),
        bounds,
        px_w: px.0,
        px_h: px.1,
        rotation,
        main,
        builtin,
        vendor: if builtin { 1552 } else { 12 },
        model: if builtin { 41152 } else { 25003 },
        serial: if builtin { 0 } else { 16843009 },
    }
}

pub fn builtin(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "Built-in Retina Display".into(),
        screen: screen(1, "Built-in Retina Display", Rect { x: 0.0, y: 0.0, w: 1728.0, h: 1117.0 }, (3456, 2234), 0.0, true, true),
    }
}

pub fn len_portrait(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "LEN T23i-10 (2)".into(),
        screen: screen(5, "LEN T23i-10", Rect { x: -1080.0, y: -803.0, w: 1080.0, h: 1920.0 }, (1080, 1920), 270.0, false, false),
    }
}

pub fn len_landscape(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "LEN T23i-10 (1)".into(),
        screen: screen(6, "LEN T23i-10", Rect { x: 1728.0, y: 0.0, w: 1920.0, h: 1080.0 }, (1920, 1080), 0.0, false, false),
    }
}

/// portrait (monitor 1, leftmost) | built-in (2) | landscape (3), as attached today.
pub fn desk3() -> Vec<Display> {
    vec![len_portrait(1), builtin(2), len_landscape(3)]
}

use crate::aero::{Aero, Cmd};
use crate::config::Axis;
use crate::world::{AeroMonitor, Snapshot, WindowRow, WorkspaceRow};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct FakeWs {
    pub name: String,
    pub monitor: u32,
    pub visible: bool,
    pub windows: Vec<u32>,
    pub root: Axis,
}

/// In-memory AeroSpace with the move semantics of MoveWorkspaceToMonitorCommand.swift:15-21
/// (same-monitor no-op; target shows the moved workspace; source gets a stub).
pub struct FakeAero {
    pub monitors: Vec<AeroMonitor>,
    pub ws: Vec<FakeWs>,
    pub focused: String,
    pub log: Vec<Cmd>,
    pub refuse: BTreeSet<String>,
    pub ignore_layout: bool,
    /// Make `windows()` fail, as a socket blip does mid-pass.
    pub fail_windows: bool,
}

impl FakeAero {
    /// `ws` rows are (name, monitor, visible, window count, root axis). Monitors take `ds`
    /// order as NSScreen order (nsscreen = index + 1).
    pub fn new(ds: &[Display], ws: &[(&str, u32, bool, usize, Axis)], focused: &str) -> Self {
        let mut next_id = 100;
        FakeAero {
            monitors: ds
                .iter()
                .enumerate()
                .map(|(i, d)| AeroMonitor { id: d.aero_id, name: d.aero_name.clone(), nsscreen: i + 1 })
                .collect(),
            ws: ws
                .iter()
                .map(|(name, monitor, visible, n, root)| FakeWs {
                    name: name.to_string(),
                    monitor: *monitor,
                    visible: *visible,
                    windows: (0..*n)
                        .map(|_| {
                            next_id += 1;
                            next_id
                        })
                        .collect(),
                    root: *root,
                })
                .collect(),
            focused: focused.into(),
            log: vec![],
            refuse: BTreeSet::new(),
            ignore_layout: false,
            fail_windows: false,
        }
    }

    pub fn ws(&self, name: &str) -> &FakeWs {
        self.ws.iter().find(|w| w.name == name).expect("no such fake workspace")
    }

    /// The workspace rows, without borrowing `self` mutably.
    pub fn clone_rows(&self) -> Vec<WorkspaceRow> {
        self.ws
            .iter()
            .map(|w| WorkspaceRow { workspace: w.name.clone(), monitor: w.monitor, visible: w.visible, focused: w.name == self.focused })
            .collect()
    }

    fn idx(&self, name: &str) -> Result<usize, String> {
        self.ws.iter().position(|w| w.name == name).ok_or_else(|| format!("no workspace {name}"))
    }

    fn resolve_target(&self, t: &str) -> Option<u32> {
        if let Ok(id) = t.parse() {
            return Some(id);
        }
        let name = t.strip_prefix('^')?.strip_suffix('$')?.replace('\\', "");
        self.monitors.iter().find(|m| m.name == name).map(|m| m.id)
    }

    fn show(&mut self, i: usize) {
        let m = self.ws[i].monitor;
        for w in self.ws.iter_mut().filter(|w| w.monitor == m) {
            w.visible = false;
        }
        self.ws[i].visible = true;
    }
}

impl Aero for FakeAero {
    fn monitors(&mut self) -> anyhow::Result<Vec<AeroMonitor>> {
        Ok(self.monitors.clone())
    }

    fn windows(&mut self) -> anyhow::Result<Snapshot> {
        if self.fail_windows {
            anyhow::bail!("list-windows: connection refused");
        }
        let mut windows = vec![];
        for w in &self.ws {
            let layout = if w.root == Axis::H { "h_tiles" } else { "v_tiles" };
            for id in &w.windows {
                windows.push(WindowRow { id: *id, workspace: w.name.clone(), monitor: w.monitor, root_layout: layout.into() });
            }
        }
        Ok(Snapshot { windows })
    }

    fn workspaces(&mut self) -> anyhow::Result<Vec<WorkspaceRow>> {
        Ok(self.clone_rows())
    }

    fn run(&mut self, cmd: &Cmd) -> Result<(), String> {
        self.log.push(cmd.clone());
        match cmd {
            Cmd::Move { ws, target, .. } => {
                if self.refuse.contains(ws) {
                    return Err(format!("Workspace '{ws}' is force-assigned to another monitor"));
                }
                let to = self.resolve_target(target).ok_or("no such monitor")?;
                let i = self.idx(ws)?;
                let from = self.ws[i].monitor;
                if from == to {
                    return Ok(());
                }
                self.ws[i].monitor = to;
                self.show(i);
                if !self.ws.iter().any(|w| w.monitor == from && w.visible)
                    && let Some(stub) = self.ws.iter().position(|w| w.monitor == from)
                {
                    self.show(stub);
                }
                Ok(())
            }
            Cmd::Flatten { ws } => self.idx(ws).map(|_| ()),
            Cmd::Layout { ws, axis } => {
                let i = self.idx(ws)?;
                if !self.ignore_layout {
                    self.ws[i].root = *axis;
                }
                Ok(())
            }
            Cmd::Focus { ws } => {
                let i = self.idx(ws)?;
                self.show(i);
                self.focused = ws.clone();
                Ok(())
            }
        }
    }
}
