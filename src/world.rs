//! AeroSpace's view of the world (`list-*` JSON rows) and its join to CoreGraphics.

use crate::config::Axis;
use crate::display::{Display, Screen};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MONITOR_FORMAT: &str = "%{monitor-id} %{monitor-name} %{monitor-appkit-nsscreen-screens-id}";
pub const WINDOW_FORMAT: &str = "%{window-id} %{workspace} %{monitor-id} %{workspace-root-container-layout}";
/// Exclusive tiling's read. `%{window-layout}` is `floating` for a floating window, a tiling
/// layout (`h_tiles`, ...) for a tiled one, else `sticky`, `macos_native_*`, ...
pub const DETAIL_FORMAT: &str = "%{window-id} %{workspace} %{app-bundle-id} %{window-title} %{window-layout}";
/// Deliberately without `%{workspace-root-container-layout}`: through list-workspaces that
/// read creates an empty workspace's root container and freezes its orientation.
pub const WORKSPACE_FORMAT: &str = "%{workspace} %{monitor-id} %{workspace-is-visible} %{workspace-is-focused}";

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AeroMonitor {
    #[serde(rename = "monitor-id")]
    pub id: u32,
    #[serde(rename = "monitor-name")]
    pub name: String,
    /// 1-based index into `NSScreen.screens`.
    #[serde(rename = "monitor-appkit-nsscreen-screens-id")]
    pub nsscreen: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WindowRow {
    #[serde(rename = "window-id")]
    pub id: u32,
    pub workspace: String,
    #[serde(rename = "monitor-id")]
    pub monitor: u32,
    #[serde(rename = "workspace-root-container-layout")]
    pub root_layout: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WinInfo {
    #[serde(rename = "window-id")]
    pub id: u32,
    pub workspace: String,
    #[serde(rename = "app-bundle-id")]
    pub app: String,
    #[serde(rename = "window-title")]
    pub title: String,
    #[serde(rename = "window-layout")]
    pub layout: String,
}

impl WinInfo {
    pub fn tiled(&self) -> bool {
        axis_of(&self.layout).is_some()
    }
    /// Floating proper: sticky, minimized and native-fullscreen windows are neither.
    pub fn floating(&self) -> bool {
        self.layout == "floating"
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WorkspaceRow {
    pub workspace: String,
    #[serde(rename = "monitor-id")]
    pub monitor: u32,
    #[serde(rename = "workspace-is-visible")]
    pub visible: bool,
    #[serde(rename = "workspace-is-focused")]
    pub focused: bool,
}

/// One atomic `list-windows --all` read (D17 step 1): the only source of workspace→monitor
/// for non-empty workspaces and of root layout. Empty workspaces are absent by construction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub windows: Vec<WindowRow>,
}

impl Snapshot {
    pub fn monitor_of(&self, ws: &str) -> Option<u32> {
        self.windows.iter().find(|w| w.workspace == ws).map(|w| w.monitor)
    }
    pub fn is_nonempty(&self, ws: &str) -> bool {
        self.windows.iter().any(|w| w.workspace == ws)
    }
    pub fn nonempty(&self) -> BTreeSet<String> {
        self.windows.iter().map(|w| w.workspace.clone()).collect()
    }
    pub fn root_axis(&self, ws: &str) -> Option<Axis> {
        self.windows.iter().find(|w| w.workspace == ws).and_then(|w| axis_of(&w.root_layout))
    }
}

pub fn axis_of(layout: &str) -> Option<Axis> {
    if layout.starts_with("h_") {
        Some(Axis::H)
    } else if layout.starts_with("v_") {
        Some(Axis::V)
    } else {
        None
    }
}

pub fn visible_on(rows: &[WorkspaceRow], monitor: u32) -> Option<&str> {
    rows.iter().find(|r| r.monitor == monitor && r.visible).map(|r| r.workspace.as_str())
}

pub fn focused(rows: &[WorkspaceRow]) -> Option<&WorkspaceRow> {
    rows.iter().find(|r| r.focused)
}

pub fn row<'a>(rows: &'a [WorkspaceRow], ws: &str) -> Option<&'a WorkspaceRow> {
    rows.iter().find(|r| r.workspace == ws)
}

/// Join AeroSpace monitors to NSScreen-ordered screens (plan P-1). An error here is the
/// D17 gate saying "AeroSpace and the OS disagree; do not act yet".
pub fn join(mons: &[AeroMonitor], screens: &[Screen]) -> Result<Vec<Display>, String> {
    if mons.len() != screens.len() {
        return Err(format!("AeroSpace sees {} monitors, AppKit {}", mons.len(), screens.len()));
    }
    let mut out = Vec::with_capacity(mons.len());
    for m in mons {
        let s = m
            .nsscreen
            .checked_sub(1)
            .and_then(|i| screens.get(i))
            .ok_or_else(|| format!("monitor {} ({}) names NSScreen #{}, which does not exist", m.id, m.name, m.nsscreen))?;
        if !m.name.starts_with(&s.name) {
            return Err(format!("monitor {} is {:?} in AeroSpace but NSScreen #{} is {:?}", m.id, m.name, m.nsscreen, s.name));
        }
        out.push(Display { aero_id: m.id, aero_name: m.name.clone(), screen: s.clone() });
    }
    out.sort_by_key(|d| d.aero_id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::desk3;

    const MONITORS: &str = r#"[
      {"monitor-appkit-nsscreen-screens-id": 3, "monitor-id": 1, "monitor-name": "LEN T23i-10 (2)"},
      {"monitor-appkit-nsscreen-screens-id": 1, "monitor-id": 2, "monitor-name": "Built-in Retina Display"},
      {"monitor-appkit-nsscreen-screens-id": 2, "monitor-id": 3, "monitor-name": "LEN T23i-10 (1)"}
    ]"#;
    const WINDOWS: &str = r#"[
      {"monitor-id": 2, "window-id": 335, "workspace": "A", "workspace-root-container-layout": "v_accordion"},
      {"monitor-id": 3, "window-id": 6590, "workspace": "2", "workspace-root-container-layout": "h_accordion"},
      {"monitor-id": 3, "window-id": 472, "workspace": "C", "workspace-root-container-layout": "h_tiles"}
    ]"#;
    const WORKSPACES: &str = r#"[
      {"monitor-id": 1, "workspace": "1", "workspace-is-focused": false, "workspace-is-visible": true},
      {"monitor-id": 3, "workspace": "2", "workspace-is-focused": false, "workspace-is-visible": true},
      {"monitor-id": 2, "workspace": "T", "workspace-is-focused": true, "workspace-is-visible": true},
      {"monitor-id": 2, "workspace": "A", "workspace-is-focused": false, "workspace-is-visible": false}
    ]"#;

    /// NSScreen order as AppKit reports it: built-in first (menu bar), then the two LENs.
    fn screens() -> Vec<Screen> {
        let d = desk3();
        vec![d[1].screen.clone(), d[2].screen.clone(), d[0].screen.clone()]
    }

    #[test]
    fn parses_aerospace_json() {
        let m: Vec<AeroMonitor> = serde_json::from_str(MONITORS).unwrap();
        assert_eq!(m[0], AeroMonitor { id: 1, name: "LEN T23i-10 (2)".into(), nsscreen: 3 });
        let s = Snapshot { windows: serde_json::from_str(WINDOWS).unwrap() };
        assert_eq!(s.monitor_of("2"), Some(3));
        assert_eq!(s.monitor_of("1"), None); // empty workspace: not in list-windows
        assert_eq!(s.root_axis("A"), Some(Axis::V));
        assert_eq!(s.nonempty().into_iter().collect::<Vec<_>>(), ["2", "A", "C"]);
        let rows: Vec<WorkspaceRow> = serde_json::from_str(WORKSPACES).unwrap();
        assert_eq!(visible_on(&rows, 2), Some("T"));
        assert_eq!(focused(&rows).unwrap().workspace, "T");
        assert_eq!(row(&rows, "A").unwrap().monitor, 2);
    }

    #[test]
    fn parses_window_details() {
        let d: Vec<WinInfo> = serde_json::from_str(
            r#"[{"window-id": 4211, "workspace": "T", "app-bundle-id": "net.kovidgoyal.kitty", "window-title": "noomz: ~", "window-layout": "h_tiles"},
                {"window-id": 5120, "workspace": "T", "app-bundle-id": "com.apple.Preview", "window-title": "x.pdf", "window-layout": "floating"},
                {"window-id": 12, "workspace": "T", "app-bundle-id": "com.bitwarden.desktop", "window-title": "", "window-layout": "sticky"}]"#,
        )
        .unwrap();
        assert!(d[0].tiled() && !d[0].floating());
        assert!(d[1].floating() && !d[1].tiled());
        assert!(!d[2].floating() && !d[2].tiled(), "sticky is neither");
    }

    #[test]
    fn join_maps_nsscreen_index_to_screen() {
        let m: Vec<AeroMonitor> = serde_json::from_str(MONITORS).unwrap();
        let ds = join(&m, &screens()).unwrap();
        assert_eq!(ds, desk3());
    }

    #[test]
    fn join_rejects_count_and_name_disagreement() {
        let m: Vec<AeroMonitor> = serde_json::from_str(MONITORS).unwrap();
        assert!(join(&m, &screens()[..2]).unwrap_err().contains("AeroSpace sees 3 monitors, AppKit 2"));
        let mut swapped = screens();
        swapped.swap(0, 1);
        assert!(join(&m, &swapped).is_err());
        let bad = [AeroMonitor { id: 1, name: "X".into(), nsscreen: 0 }];
        assert!(join(&bad, &screens()[..1]).is_err());
    }

    #[test]
    fn workspace_format_never_reads_root_layout() {
        // Global constraint: reading root layout through list-workspaces creates the root
        // container of an empty workspace (WorkspaceEx.swift:4-13).
        assert!(!WORKSPACE_FORMAT.contains("root-container"));
    }
}
