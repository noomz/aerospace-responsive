//! Daemon state that must survive a restart (pins, profile lock — plan P-7) and the
//! warning set surfaced by `status` (D25 V5, cleared by cause — plan P-10).

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
/// pass (recomputed), Pass on the next clean pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    Config,
    World,
    Pass,
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
        let p = Persisted { pins: vec![pin("1")], lock: Some("desk".into()) };
        p.save(&path).unwrap();
        assert_eq!(Persisted::load(&path), p);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(Persisted::load(&path), Persisted::default());
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
