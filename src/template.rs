//! Workspace templates (SPEC "workspace template" §G): a per-workspace rule naming the tiling
//! tree to rebuild. This module resolves which template applies to which workspace.

use crate::config::{Axis, Config, Node, Pred, Profile, TemplateBlock};
use crate::world::WinInfo;
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

/// V38: window ids per slot, depth-first slot order (empty slots kept), plus the rest.
/// Every list is lowest id first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub slots: Vec<Vec<u32>>,
    pub rest: Vec<u32>,
}

/// V38: each tiled window goes to the first slot (depth-first) whose `app` equals its bundle
/// id and whose `title`, if set, matches; no match → rest. Floating windows are not counted (C23).
pub fn assign(tree: &[Node], windows: &[WinInfo]) -> Assignment {
    fn slots<'a>(nodes: &'a [Node], out: &mut Vec<&'a crate::config::Slot>) {
        for n in nodes {
            match n {
                Node::Slot(s) => out.push(s),
                Node::Container(c) => slots(c, out),
            }
        }
    }
    let mut flat = Vec::new();
    slots(tree, &mut flat);
    let mut a = Assignment { slots: vec![Vec::new(); flat.len()], rest: Vec::new() };
    let mut tiled: Vec<&WinInfo> = windows.iter().filter(|w| w.tiled()).collect();
    tiled.sort_by_key(|w| w.id);
    for w in tiled {
        match flat.iter().position(|s| s.app == w.app && s.title.as_ref().is_none_or(|t| t.is_match(&w.title))) {
            Some(i) => a.slots[i].push(w.id),
            None => a.rest.push(w.id),
        }
    }
    a
}

/// A tiling tree to build. The root is always a `Split`; its kind is tiles (V39).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tree {
    Win(u32),
    Split(Axis, Vec<Tree>),
}

/// V39: the canonical tree for `root` + `tree` filled by `a`. A slot's windows sit side by side
/// in the slot's container, rest windows go to the end of the root. The result has no empty or
/// one-child container below the root and no same-axis nesting; a root whose only child is a
/// container takes that container's axis and children.
pub fn canonical(root: Axis, tree: &[Node], a: &Assignment) -> Tree {
    fn fill(axis: Axis, nodes: &[Node], a: &Assignment, next_slot: &mut usize) -> Tree {
        let mut kids = Vec::new();
        for n in nodes {
            match n {
                Node::Slot(_) => {
                    kids.extend(a.slots[*next_slot].iter().map(|&id| Tree::Win(id)));
                    *next_slot += 1;
                }
                Node::Container(c) => kids.push(fill(axis.opposite(), c, a, next_slot)),
            }
        }
        Tree::Split(axis, kids)
    }
    let Tree::Split(_, mut kids) = fill(root, tree, a, &mut 0) else { unreachable!() };
    kids.extend(a.rest.iter().map(|&id| Tree::Win(id)));
    normalize_root(root, kids)
}

/// Steps (1)–(3) of V39 bottom-up, so children are canonical before their parent looks at them;
/// then step (4) at the root. One pass reaches the fixed point: a spliced or promoted container
/// is already canonical.
fn normalize_root(axis: Axis, kids: Vec<Tree>) -> Tree {
    let mut kids = normalize_kids(axis, kids);
    let mut axis = axis;
    while let [Tree::Split(..)] = kids.as_slice() {
        let Some(Tree::Split(a, k)) = kids.pop() else { unreachable!() };
        (axis, kids) = (a, k);
    }
    Tree::Split(axis, kids)
}

fn normalize_kids(axis: Axis, kids: Vec<Tree>) -> Vec<Tree> {
    let mut out = Vec::new();
    for k in kids {
        let k = match k {
            Tree::Split(a, sub) => {
                let mut sub = normalize_kids(a, sub);
                match sub.len() {
                    0 => continue,
                    1 => sub.pop().unwrap(),
                    _ => Tree::Split(a, sub),
                }
            }
            w => w,
        };
        match k {
            Tree::Split(a, sub) if a == axis => out.extend(sub),
            k => out.push(k),
        }
    }
    out
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

    fn tpl(root: &str, tree: &str) -> Template {
        let src = format!("[[workspace-template]]\nworkspaces = [\"X\"]\nroot = \"{root}\"\ntree = {tree}\n");
        effective(&parse(&src).unwrap().config, None).remove(0)
    }

    fn win(id: u32, app: &str, title: &str, layout: &str) -> WinInfo {
        WinInfo { id, workspace: "X".into(), app: app.into(), app_name: app.into(), title: title.into(), layout: layout.into() }
    }

    fn tiled(id: u32, app: &str) -> WinInfo {
        win(id, app, "", "h_tiles")
    }

    fn built(t: &Template, ws: &[WinInfo]) -> Tree {
        canonical(t.root, &t.tree, &assign(&t.tree, ws))
    }

    use Tree::{Split, Win};

    #[test]
    fn first_slot_wins_and_title_narrows() {
        let t = tpl("h", r#"[{ app = "m", title = "^inbox" }, { app = "m" }, { app = "m", title = "never" }]"#);
        let ws = [win(3, "m", "Inbox (2)", "h_tiles"), win(1, "m", "Drafts", "h_tiles"), win(2, "m", "inbox", "v_tiles")];
        assert_eq!(assign(&t.tree, &ws), Assignment { slots: vec![vec![2, 3], vec![1], vec![]], rest: vec![] });
        // Depth-first: a nested slot comes before a later top-level one.
        let t = tpl("h", r#"[[{ app = "x" }, { app = "m" }], { app = "m" }]"#);
        assert_eq!(assign(&t.tree, &[tiled(1, "m")]), Assignment { slots: vec![vec![], vec![1], vec![]], rest: vec![] });
    }

    #[test]
    fn many_windows_in_a_slot_sit_side_by_side_lowest_id_first() {
        let t = tpl("h", r#"[{ app = "a" }, [{ app = "b" }, { app = "c" }]]"#);
        let ws = [tiled(9, "b"), tiled(4, "b"), tiled(1, "a"), tiled(5, "c")];
        assert_eq!(built(&t, &ws), Split(Axis::H, vec![Win(1), Split(Axis::V, vec![Win(4), Win(9), Win(5)])]));
    }

    #[test]
    fn rest_goes_to_the_end_of_the_root_and_floating_is_not_counted() {
        let t = tpl("h", r#"[{ app = "a" }, [{ app = "b" }, { app = "c" }]]"#);
        let ws = [tiled(7, "z"), tiled(2, "y"), win(3, "a", "", "floating"), tiled(1, "a"), tiled(5, "b"), tiled(6, "c")];
        let a = assign(&t.tree, &ws);
        assert_eq!(a, Assignment { slots: vec![vec![1], vec![5], vec![6]], rest: vec![2, 7] });
        assert_eq!(canonical(t.root, &t.tree, &a), Split(Axis::H, vec![Win(1), Split(Axis::V, vec![Win(5), Win(6)]), Win(2), Win(7)]));
    }

    #[test]
    fn empty_container_is_dropped_and_one_child_container_collapses() {
        let t = tpl("h", r#"[{ app = "a" }, [{ app = "b" }, { app = "c" }], { app = "d" }]"#);
        assert_eq!(built(&t, &[tiled(1, "a"), tiled(4, "d")]), Split(Axis::H, vec![Win(1), Win(4)]));
        assert_eq!(built(&t, &[tiled(1, "a"), tiled(3, "c"), tiled(4, "d")]), Split(Axis::H, vec![Win(1), Win(3), Win(4)]));
    }

    #[test]
    fn collapse_exposes_same_axis_nesting_which_merges_in_place() {
        // h[a, v[h[b, c]], d]: v has one child, so h[b, c] lands inside the h root and merges.
        let t = tpl("h", r#"[{ app = "a" }, [[{ app = "b" }, { app = "c" }]], { app = "d" }]"#);
        let ws = [tiled(1, "a"), tiled(2, "b"), tiled(3, "c"), tiled(4, "d")];
        assert_eq!(built(&t, &ws), Split(Axis::H, vec![Win(1), Win(2), Win(3), Win(4)]));
        // One level down: v[h[v[b, c]], d] -> the h collapses, its v merges into the outer v.
        let t = tpl("h", r#"[{ app = "a" }, [[[{ app = "b" }, { app = "c" }]], { app = "d" }]]"#);
        let ws = [tiled(1, "a"), tiled(2, "b"), tiled(3, "c"), tiled(4, "d")];
        assert_eq!(built(&t, &ws), Split(Axis::H, vec![Win(1), Split(Axis::V, vec![Win(2), Win(3), Win(4)])]));
        // An opposite-axis child stays nested: v[h[b], h[c, e]] -> v[b, h[c, e]].
        let t = tpl("h", r#"[{ app = "a" }, [[{ app = "b" }], [{ app = "c" }, { app = "e" }]]]"#);
        let ws = [tiled(1, "a"), tiled(2, "b"), tiled(3, "c"), tiled(5, "e")];
        assert_eq!(built(&t, &ws), Split(Axis::H, vec![Win(1), Split(Axis::V, vec![Win(2), Split(Axis::H, vec![Win(3), Win(5)])])]));
    }

    #[test]
    fn root_takes_the_axis_of_its_only_container() {
        let t = tpl("h", r#"[{ app = "a" }, [{ app = "b" }, [{ app = "c" }, { app = "d" }]]]"#);
        assert_eq!(built(&t, &[tiled(2, "b"), tiled(3, "c"), tiled(4, "d")]), Split(Axis::V, vec![Win(2), Split(Axis::H, vec![Win(3), Win(4)])]));
        // A rest window keeps the root two-child, so no promotion.
        let ws = [tiled(2, "b"), tiled(3, "c"), tiled(9, "z")];
        assert_eq!(built(&t, &ws), Split(Axis::H, vec![Split(Axis::V, vec![Win(2), Win(3)]), Win(9)]));
    }

    #[test]
    fn single_window_and_empty_workspace() {
        let t = tpl("v", r#"[[{ app = "a" }, { app = "b" }]]"#);
        assert_eq!(built(&t, &[tiled(1, "b")]), Split(Axis::V, vec![Win(1)]));
        assert_eq!(built(&t, &[]), Split(Axis::V, vec![]));
    }

    #[test]
    fn motivating_case_and_canonical_is_a_fixed_point() {
        let t = tpl("h", r#"[{ app = "line" }, [{ app = "igcl" }, { app = "msgr" }]]"#);
        let ws = [tiled(49, "msgr"), tiled(10, "igcl"), tiled(44, "line")];
        let tree = built(&t, &ws);
        assert_eq!(tree, Split(Axis::H, vec![Win(44), Split(Axis::V, vec![Win(10), Win(49)])]));
        let Split(axis, kids) = tree.clone() else { unreachable!() };
        assert_eq!(normalize_root(axis, kids), tree);
    }

    #[test]
    fn no_blocks_no_templates() {
        assert!(effective(&parse("").unwrap().config, None).is_empty());
    }
}
