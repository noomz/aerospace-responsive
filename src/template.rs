//! Workspace templates (SPEC "workspace template" §G): a per-workspace rule naming the tiling
//! tree to rebuild. This module resolves which template applies to which workspace.

use crate::config::{Axis, Config, Node, Pred, Profile, TemplateBlock};
use std::collections::BTreeMap;

/// A workspace's effective template. Rule identity = workspace name (V48).
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub workspace: String,
    pub when: Option<Pred>,
    pub root: Axis,
    pub tree: Vec<Node>,
    /// The block it came from: `top-level #1`, `profile desk3 #1`.
    pub source: String,
}

/// V48: per workspace the last block wins whole, `when` included: top-level blocks in file
/// order, then the active profile's. No per-field merge and no fallback to an earlier block
/// whose `when` would match; `when` is judged later, against the landed monitor (V42).
pub fn effective(cfg: &Config, profile: Option<&Profile>) -> Vec<Template> {
    let label = |at: String, blocks: &[TemplateBlock]| -> Vec<(String, TemplateBlock)> {
        blocks.iter().enumerate().map(|(i, b)| (format!("{at} #{}", i + 1), b.clone())).collect()
    };
    let mut blocks = label("top-level".into(), &cfg.workspace_template);
    if let Some(p) = profile {
        blocks.extend(label(format!("profile {}", p.name), &p.workspace_template));
    }
    let mut out: BTreeMap<String, Template> = BTreeMap::new();
    for (source, b) in blocks {
        for ws in &b.workspaces {
            let t = Template { workspace: ws.clone(), when: b.when.clone(), root: b.root, tree: b.tree.clone(), source: source.clone() };
            out.insert(ws.clone(), t);
        }
    }
    out.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;

    fn get<'a>(ts: &'a [Template], ws: &str) -> &'a Template {
        ts.iter().find(|t| t.workspace == ws).unwrap_or_else(|| panic!("no template for {ws}"))
    }

    const TWO_BLOCKS: &str = r#"
[[workspace-template]]
workspaces = ["C", "D"]
root = "h"
tree = [{ app = "a" }, { app = "b" }]

[[workspace-template]]
workspaces = ["C"]
when = { builtin = true }
root = "v"
tree = [{ app = "z" }]

[[profile]]
name = "desk3"
when = {}

[[profile.workspace-template]]
workspaces = ["D"]
root = "v"
tree = [{ app = "p" }]
"#;

    #[test]
    fn one_block_names_many_workspaces_with_one_source() {
        let cfg = parse("[[workspace-template]]\nworkspaces = [\"C\", \"D\"]\nroot = \"h\"\ntree = [{ app = \"a\" }]\n").unwrap().config;
        let ts = effective(&cfg, None);
        assert_eq!(ts.len(), 2);
        assert_eq!((get(&ts, "C").source.as_str(), get(&ts, "D").source.as_str()), ("top-level #1", "top-level #1"));
        assert_eq!(get(&ts, "C").tree, get(&ts, "D").tree);
    }

    #[test]
    fn last_block_wins_whole_when_included_and_names_its_source() {
        let cfg = parse(TWO_BLOCKS).unwrap().config;
        let ts = effective(&cfg, None);
        let c = get(&ts, "C");
        assert_eq!((c.root, c.source.as_str()), (Axis::V, "top-level #2"));
        assert_eq!(c.tree.len(), 1);
        // Block #1's `when`-less template does not survive as a fallback for other monitors.
        assert_eq!(c.when.as_ref().and_then(|w| w.builtin), Some(true));
        assert_eq!(get(&ts, "D").source, "top-level #1", "block #2 leaves D alone");
    }

    #[test]
    fn the_active_profile_comes_after_top_level() {
        let cfg = parse(TWO_BLOCKS).unwrap().config;
        let ts = effective(&cfg, Some(&cfg.profile[0]));
        let d = get(&ts, "D");
        assert_eq!((d.root, d.source.as_str()), (Axis::V, "profile desk3 #1"));
        assert_eq!(get(&ts, "C").source, "top-level #2", "profile leaves C alone");
        assert_eq!(get(&effective(&cfg, None), "D").source, "top-level #1", "inactive profile adds nothing");
    }

    #[test]
    fn no_blocks_no_templates() {
        assert!(effective(&parse("").unwrap().config, None).is_empty());
    }
}
