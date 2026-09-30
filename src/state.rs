//! Daemon state that must survive a restart (pins, profile lock — plan P-7) and the
//! warning set surfaced by `status` (D25 V5, cleared by cause — plan P-10).

use crate::config::{ElseAction, Entry};
use crate::tiling::Rule;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A workspace pin (ticket 09), keyed to (workspace, monitor). The CG display id is stable
/// across replug (ticket 10); `monitor_name` is for display only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub workspace: String,
    pub cg_id: u32,
    pub monitor_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Persisted {
    #[serde(default)]
    pub pins: Vec<Pin>,
    /// Profile lock (02 M1, renamed by 11 V1).
    #[serde(default)]
    pub lock: Option<String>,
    #[serde(default)]
    pub exclusive_tiling: TilingMemory,
}

/// Exclusive tiling's memory (I.state): the windows the rule floated, and per workspace the
/// rule it last converged to. Survives daemon and AeroSpace restarts (V8, V9).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TilingMemory {
    /// Rule-floated windows by workspace.
    #[serde(default)]
    pub rule_floated: BTreeMap<String, BTreeSet<Floated>>,
    /// The last converged effective rule by workspace, updated only when its converge completes.
    #[serde(default)]
    pub records: BTreeMap<String, RuleRecord>,
}

/// Keyed (window id, bundle id): a `CGWindowID` is recycled after close, the bundle id tells
/// a recycled id apart (V9).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Floated {
    pub window_id: u32,
    pub app: String,
}

/// The fields of an effective rule that decide what tiles; sources are not part of it, so
/// moving a rule between blocks unchanged converges nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RuleRecord {
    pub else_action: ElseAction,
    pub tiling_allowlist: Vec<Entry>,
}

impl RuleRecord {
    pub fn of(r: &Rule) -> Self {
        RuleRecord { else_action: r.else_action, tiling_allowlist: r.tiling_allowlist.clone() }
    }

    /// The config field names that differ from `r` (for `rule changed: <fields>`).
    pub fn changed(&self, r: &Rule) -> Vec<&'static str> {
        let mut out = vec![];
        if self.else_action != r.else_action {
            out.push("else-action");
        }
        if self.tiling_allowlist != r.tiling_allowlist {
            out.push("tiling-allowlist");
        }
        out
    }
}

impl TilingMemory {
    /// The workspace `(id, app)` is rule-floated on, if any.
    pub fn floated_on(&self, id: u32, app: &str) -> Option<&str> {
        self.rule_floated.iter().find(|(_, set)| set.iter().any(|f| f.window_id == id && f.app == app)).map(|(ws, _)| ws.as_str())
    }

    pub fn insert(&mut self, ws: &str, id: u32, app: &str) {
        self.remove(id);
        self.rule_floated.entry(ws.to_string()).or_default().insert(Floated { window_id: id, app: app.to_string() });
    }

    /// Forget window `id` wherever it is. True when something was dropped.
    pub fn remove(&mut self, id: u32) -> bool {
        let mut hit = false;
        for set in self.rule_floated.values_mut() {
            let before = set.len();
            set.retain(|f| f.window_id != id);
            hit |= set.len() != before;
        }
        self.rule_floated.retain(|_, set| !set.is_empty());
        hit
    }

    /// V9: drop entries whose window is gone or whose id now belongs to another app (a
    /// recycled id). `live` = (window id, bundle id) of every window. True when it changed.
    pub fn prune(&mut self, live: &[(u32, &str)]) -> bool {
        let before = self.clone();
        for set in self.rule_floated.values_mut() {
            set.retain(|f| live.iter().any(|(id, app)| *id == f.window_id && *app == f.app));
        }
        self.rule_floated.retain(|_, set| !set.is_empty());
        *self != before
    }

    pub fn counts(&self) -> BTreeMap<String, usize> {
        self.rule_floated.iter().map(|(ws, set)| (ws.clone(), set.len())).collect()
    }
}

impl Persisted {
    /// Missing state starts empty; a corrupt file is logged, never fatal.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Err(_) => Self::default(),
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                crate::log!("state file {} is corrupt ({e}); starting empty", path.display());
                Self::default()
            }),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn pinned_names(&self) -> BTreeSet<String> {
        self.pins.iter().map(|p| p.workspace.clone()).collect()
    }

    pub fn pin(&mut self, pin: Pin) {
        self.pins.retain(|p| p.workspace != pin.workspace);
        self.pins.push(pin);
    }

    pub fn unpin(&mut self, ws: &str) -> bool {
        let before = self.pins.len();
        self.pins.retain(|p| p.workspace != ws);
        before != self.pins.len()
    }

    /// A pin clears when its workspace empties (09 G3). Returns the cleared names.
    pub fn drop_empty(&mut self, nonempty: &BTreeSet<String>) -> Vec<String> {
        let (keep, gone): (Vec<Pin>, Vec<Pin>) = self.pins.drain(..).partition(|p| nonempty.contains(&p.workspace));
        self.pins = keep;
        gone.into_iter().map(|p| p.workspace).collect()
    }
}

/// How a warning clears (plan P-10): Config on the next successful reload, World on every
/// pass (recomputed), Pass on the next clean pass, ExclusiveTiling when a subscribe child
/// starts (or the last rule goes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    Config,
    World,
    Pass,
    ExclusiveTiling,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub key: String,
    pub class: Class,
    pub message: String,
    pub since: String,
}

#[derive(Debug, Default)]
pub struct Warnings {
    items: BTreeMap<String, Warning>,
}

impl Warnings {
    /// True when the warning is new; callers log only then ("warn once").
    pub fn raise(&mut self, class: Class, key: &str, message: &str) -> bool {
        if self.items.contains_key(key) {
            return false;
        }
        self.items.insert(key.to_string(), Warning { key: key.into(), class, message: message.into(), since: now() });
        true
    }

    pub fn clear(&mut self, class: Class) {
        self.items.retain(|_, w| w.class != class);
    }

    /// Make `fresh` the complete set for `class`, keeping `since` for survivors.
    /// Returns the messages that are new.
    pub fn replace(&mut self, class: Class, fresh: Vec<(String, String)>) -> Vec<String> {
        let keys: BTreeSet<String> = fresh.iter().map(|(k, _)| k.clone()).collect();
        self.items.retain(|k, w| w.class != class || keys.contains(k));
        let mut new = vec![];
        for (k, m) in fresh {
            if self.raise(class, &k, &m) {
                new.push(m);
            }
        }
        new
    }

    pub fn list(&self) -> Vec<Warning> {
        self.items.values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub fn now() -> String {
    humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(ws: &str) -> Pin {
        Pin { workspace: ws.into(), cg_id: 5, monitor_name: "LEN T23i-10 (2)".into() }
    }

    #[test]
    fn pins_replace_unpin_and_clear_when_empty() {
        let mut p = Persisted::default();
        p.pin(pin("1"));
        p.pin(Pin { cg_id: 6, ..pin("1") });
        assert_eq!(p.pins.len(), 1);
        assert_eq!(p.pins[0].cg_id, 6);
        p.pin(pin("A"));
        let nonempty: BTreeSet<String> = ["A".to_string()].into();
        assert_eq!(p.drop_empty(&nonempty), ["1"]);
        assert!(p.unpin("A"));
        assert!(!p.unpin("A"));
    }

    #[test]
    fn state_round_trips_and_survives_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert_eq!(Persisted::load(&path), Persisted::default());
        let mut p = Persisted { pins: vec![pin("1")], lock: Some("desk".into()), ..Persisted::default() };
        p.exclusive_tiling.insert("T", 4380, "net.kovidgoyal.kitty");
        p.save(&path).unwrap();
        assert_eq!(Persisted::load(&path), p);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(Persisted::load(&path), Persisted::default());
    }

    #[test]
    fn state_without_tiling_memory_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"pins":[],"lock":"desk"}"#).unwrap();
        let p = Persisted::load(&path);
        assert_eq!(p.lock.as_deref(), Some("desk"));
        assert_eq!(p.exclusive_tiling, TilingMemory::default());
    }

    #[test]
    fn prune_drops_gone_and_recycled_windows() {
        let mut m = TilingMemory::default();
        m.insert("T", 1, "com.apple.Safari");
        m.insert("T", 2, "com.apple.Preview");
        m.insert("X", 3, "com.apple.Finder");
        // 1 still Safari; 2 recycled by kitty; 3 closed.
        assert!(m.prune(&[(1, "com.apple.Safari"), (2, "net.kovidgoyal.kitty")]));
        assert_eq!(m.counts(), BTreeMap::from([("T".to_string(), 1)]));
        assert_eq!(m.floated_on(1, "com.apple.Safari"), Some("T"));
        assert!(!m.prune(&[(1, "com.apple.Safari")]), "nothing left to prune");
    }

    #[test]
    fn a_window_is_rule_floated_on_one_workspace_at_most() {
        let mut m = TilingMemory::default();
        m.insert("T", 7, "a");
        m.insert("X", 7, "a");
        assert_eq!(m.floated_on(7, "a"), Some("X"));
        assert!(m.remove(7));
        assert!(m.rule_floated.is_empty());
        assert!(!m.remove(7));
    }

    #[test]
    fn warnings_raise_once_and_clear_by_class() {
        let mut w = Warnings::default();
        assert!(w.raise(Class::Pass, "mismatch", "x"));
        assert!(!w.raise(Class::Pass, "mismatch", "x"));
        let fresh = vec![("unsatisfiable:1".to_string(), "a".to_string()), ("unsatisfiable:2".to_string(), "b".to_string())];
        assert_eq!(w.replace(Class::World, fresh.clone()), ["a", "b"]);
        assert!(w.replace(Class::World, fresh[..1].to_vec()).is_empty());
        assert_eq!(w.list().len(), 2); // mismatch + unsatisfiable:1
        w.clear(Class::Pass);
        assert_eq!(w.list().len(), 1);
        w.clear(Class::World);
        assert!(w.is_empty());
    }
}
