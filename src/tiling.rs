//! Exclusive tiling (SPEC §G): on a named workspace only windows its tiling allowlist admits,
//! within each entry's tile limit, tile; every other window arriving tiled gets the else-action.
//! Pure: the worker feeds it window reads and executes what it returns.

use crate::config::{Config, ElseAction, Entry, Profile, TilingBlock};
use serde::Serialize;
use std::collections::BTreeMap;

/// One field's cascaded value and the block it came from (`top-level #1`, `profile desk3 #1`).
pub type Sourced<T> = (T, String);

/// Per-workspace fields as the cascade left them; `None` = no block set the field.
#[derive(Debug, Clone, Default)]
pub struct Fields {
    pub else_action: Option<Sourced<ElseAction>>,
    pub tiling_allowlist: Option<Sourced<Vec<Entry>>>,
}

/// V16: per workspace per field, the last block setting it wins — top-level blocks in file
/// order, then the active profile's. `tiling-allowlist` is one field, replaced whole.
pub fn cascade(cfg: &Config, profile: Option<&Profile>) -> BTreeMap<String, Fields> {
    let label = |at: &str, blocks: &[TilingBlock]| -> Vec<(String, TilingBlock)> {
        blocks.iter().enumerate().map(|(i, b)| (format!("{at} #{}", i + 1), b.clone())).collect()
    };
    let mut blocks = label("top-level", &cfg.exclusive_tiling);
    if let Some(p) = profile {
        blocks.extend(label(&format!("profile {}", p.name), &p.exclusive_tiling));
    }
    let mut out: BTreeMap<String, Fields> = BTreeMap::new();
    for (src, b) in blocks {
        for ws in &b.workspaces {
            let f = out.entry(ws.clone()).or_default();
            if let Some(a) = b.else_action {
                f.else_action = Some((a, src.clone()));
            }
            if let Some(l) = &b.tiling_allowlist {
                f.tiling_allowlist = Some((l.clone(), src.clone()));
            }
        }
    }
    out
}

/// A workspace's effective rule. Rule identity = workspace name (V16).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub workspace: String,
    pub else_action: ElseAction,
    /// `default` when no block sets it.
    pub else_action_source: String,
    pub tiling_allowlist: Vec<Entry>,
    pub tiling_allowlist_source: String,
}

/// Effective rules for this setup, by workspace. A workspace without an allowlist is a load
/// error (`config::validate`), so none is dropped here in practice.
pub fn effective(cfg: &Config, profile: Option<&Profile>) -> Vec<Rule> {
    cascade(cfg, profile)
        .into_iter()
        .filter_map(|(ws, f)| {
            let (tiling_allowlist, tiling_allowlist_source) = f.tiling_allowlist?;
            let (else_action, else_action_source) = f.else_action.unwrap_or((ElseAction::Float, "default".into()));
            Some(Rule { workspace: ws, else_action, else_action_source, tiling_allowlist, tiling_allowlist_source })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;

    const CASCADE: &str = r#"
[[exclusive-tiling]]
workspaces = ["T", "X"]
tiling-allowlist = [{ app = "net.kovidgoyal.kitty", max-tiled = 1 }]

[[exclusive-tiling]]
workspaces = ["X"]
else-action = "float"
tiling-allowlist = [{ app = "com.apple.TextEdit" }]

[[profile]]
name = "desk3"
when = {}

[[profile.exclusive-tiling]]
workspaces = ["T"]
tiling-allowlist = [
  { app = "net.kovidgoyal.kitty", title = "Settings", max-tiled = 0 },
  { app = "net.kovidgoyal.kitty", max-tiled = 1 },
]
"#;

    fn rule<'r>(rules: &'r [Rule], ws: &str) -> &'r Rule {
        rules.iter().find(|r| r.workspace == ws).expect("rule")
    }

    #[test]
    fn last_block_wins_per_field_and_names_its_source() {
        let cfg = parse(CASCADE).unwrap().config;
        let top = effective(&cfg, None);
        assert_eq!(top.len(), 2);
        let x = rule(&top, "X");
        assert_eq!(x.tiling_allowlist[0].app, "com.apple.TextEdit", "allowlist replaced whole, not concatenated");
        assert_eq!(x.tiling_allowlist.len(), 1);
        assert_eq!((x.tiling_allowlist_source.as_str(), x.else_action_source.as_str()), ("top-level #2", "top-level #2"));
        let t = rule(&top, "T");
        assert_eq!((t.else_action, t.else_action_source.as_str()), (ElseAction::Float, "default"));

        let desk = effective(&cfg, Some(&cfg.profile[0]));
        let t = rule(&desk, "T");
        assert_eq!(t.tiling_allowlist.len(), 2);
        assert_eq!(t.tiling_allowlist_source, "profile desk3 #1");
        assert_eq!(t.tiling_allowlist[0].title.as_ref().unwrap().as_str(), "Settings");
        assert_eq!(t.tiling_allowlist[0].max_tiled, Some(0));
        assert_eq!(rule(&desk, "X").tiling_allowlist_source, "top-level #2", "profile leaves X alone");
    }

    #[test]
    fn title_is_case_insensitive_and_unanchored() {
        let cfg = parse(CASCADE).unwrap().config;
        let t = &cfg.profile[0].exclusive_tiling[0].tiling_allowlist.as_ref().unwrap()[0];
        let title = t.title.as_ref().unwrap();
        assert!(title.is_match("kitty settings (1)"));
        assert!(!title.is_match("noomz: ~"));
    }
}
