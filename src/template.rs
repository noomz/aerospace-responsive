//! Workspace templates (SPEC "workspace template" §G): a per-workspace rule naming the tiling
//! tree to rebuild. This module resolves which template applies to which workspace.

use crate::aero::{Cmd, Dir};
use crate::config::{Axis, Config, Kind, Node, Pred, Profile, TemplateBlock};
use crate::state::{TemplateRecord, TemplateRecords};
use crate::world::WinInfo;
use anyhow::Result;
use std::collections::BTreeMap;
use std::fmt;

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

/// V43: the commands that rebuild `t` from the workspace's windows. Empty workspace → none.
pub fn rebuild(t: &Template, windows: &[WinInfo]) -> Vec<Cmd> {
    plan(&t.workspace, &canonical(t.root, &t.tree, &assign(&t.tree, windows)))
}

/// V43 recipe (T31) for a canonical tree. It reads neither the order windows have after flatten
/// nor any most-recent child it did not set itself, and every command targets an id or `ws` (V54).
/// The axis comes from the tree, not the template: V39 step (4) may have promoted the root.
pub fn plan(ws: &str, tree: &Tree) -> Vec<Cmd> {
    let leaves = tree.leaves();
    let (Tree::Split(axis, kids), [_, rest @ ..]) = (tree, leaves.as_slice()) else { return vec![] };
    let mut cmds = vec![
        Cmd::Flatten { ws: ws.into() },
        Cmd::LayoutKind { ws: ws.into(), layout: Kind::Tiles },
        Cmd::Layout { ws: ws.into(), axis: *axis },
    ];
    // Push L2..Ln to the end in turn: on a flat root each move swaps, and at the edge it stops.
    for &id in rest {
        cmds.extend(std::iter::repeat_n(Cmd::MoveWindow { id, dir: Dir::pos(*axis) }, rest.len()));
    }
    nest(*axis, kids, &mut cmds);
    cmds
}

/// Builds each container of `kids` from its flat, contiguous leaves, then recurses into it. A
/// container takes all its leaves before any sub-container exists: a move enters a container
/// after its most-recent child, and a sub-container there (the move's own axis) would swallow it.
fn nest(parent: Axis, kids: &[Tree], cmds: &mut Vec<Cmd>) {
    for k in kids {
        let Tree::Split(axis, sub) = k else { continue };
        let leaves = k.leaves();
        // The joiner is bound last, so it is the most-recent child the next move lands after.
        cmds.push(Cmd::JoinWith { id: leaves[1], dir: Dir::neg(parent) });
        cmds.extend(leaves[2..].iter().map(|&id| Cmd::MoveWindow { id, dir: Dir::neg(parent) }));
        nest(*axis, sub, cmds);
    }
}

/// The parts of `t` that decide the built tree: `root` and `tree`, titles included. `when` and
/// the source block are left out, so moving a template between blocks, or narrowing its
/// monitor, rebuilds nothing. Plain text, so it stays stable across builds of this tool.
pub fn fingerprint(t: &Template) -> String {
    fn node(n: &Node, axis: Axis, out: &mut String) {
        match n {
            Node::Slot(s) => {
                out.push_str(&s.app);
                if let Some(re) = &s.title {
                    out.push_str(&format!(" title {:?}", re.as_str()));
                }
            }
            Node::Container(c) => nodes(axis.opposite(), c, out),
        }
    }
    fn nodes(axis: Axis, ns: &[Node], out: &mut String) {
        out.push_str(match axis {
            Axis::H => "h[",
            Axis::V => "v[",
        });
        for (i, n) in ns.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            node(n, axis, out);
        }
        out.push(']');
    }
    let mut out = String::new();
    nodes(t.root, &t.tree, &mut out);
    out
}

/// V41: the record a rebuild of `t` over `windows` would leave. Slot paths are 1-based and
/// dotted by depth (`2.1`), in the depth-first order `assign` uses.
pub fn wanted(t: &Template, windows: &[WinInfo]) -> TemplateRecord {
    fn paths(nodes: &[Node], prefix: &str, out: &mut Vec<String>) {
        for (i, n) in nodes.iter().enumerate() {
            let p = if prefix.is_empty() { (i + 1).to_string() } else { format!("{prefix}.{}", i + 1) };
            match n {
                Node::Slot(_) => out.push(p),
                Node::Container(c) => paths(c, &p, out),
            }
        }
    }
    let mut keys = Vec::new();
    paths(&t.tree, "", &mut keys);
    let a = assign(&t.tree, windows);
    TemplateRecord { fingerprint: fingerprint(t), slots: keys.into_iter().zip(a.slots).collect(), rest: a.rest }
}

/// Why a pass would rebuild (V40 (a)); the text is explain's `record:` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    FirstRun,
    TemplateChanged,
    WindowsChanged,
}

impl fmt::Display for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Pending::FirstRun => "pending: first run",
            Pending::TemplateChanged => "pending: template changed",
            Pending::WindowsChanged => "pending: windows changed",
        })
    }
}

/// V41 pass rule: no record → rebuild; equal → nothing; differs → rebuild.
pub fn pending(have: Option<&TemplateRecord>, want: &TemplateRecord) -> Option<Pending> {
    match have {
        None => Some(Pending::FirstRun),
        Some(h) if h.fingerprint != want.fingerprint => Some(Pending::TemplateChanged),
        Some(h) if h != want => Some(Pending::WindowsChanged),
        Some(_) => None,
    }
}

/// V41: runs a rebuild of `ws` so that only a completed one leaves a record. The old record is
/// removed and saved before the first command; `want` is stored and saved after the last
/// command succeeds. `run` executes one command and returns `Ok(false)` when it stopped before
/// running it (abort). A failure or abort halfway leaves no record, so the next pass rebuilds.
/// Returns true when every command ran. No commands (empty workspace) → no record.
pub fn execute(
    records: &mut TemplateRecords,
    ws: &str,
    want: TemplateRecord,
    cmds: &[Cmd],
    save: &mut dyn FnMut(&TemplateRecords),
    mut run: impl FnMut(&Cmd) -> Result<bool>,
) -> Result<bool> {
    if records.remove(ws).is_some() {
        save(records);
    }
    if cmds.is_empty() {
        return Ok(true);
    }
    for cmd in cmds {
        if !run(cmd)? {
            return Ok(false);
        }
    }
    records.insert(ws.to_string(), want);
    save(records);
    Ok(true)
}

impl Tree {
    /// Window ids, depth-first.
    pub fn leaves(&self) -> Vec<u32> {
        match self {
            Tree::Win(id) => vec![*id],
            Tree::Split(_, kids) => kids.iter().flat_map(Tree::leaves).collect(),
        }
    }
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

    // --- V43 rebuild plan, against the FakeAero tree model (T34) ---

    use crate::aero::Aero;
    use crate::testutil::{FakeAero, desk3};

    fn h(kids: Vec<Tree>) -> Tree {
        Split(Axis::H, kids)
    }

    fn v(kids: Vec<Tree>) -> Tree {
        Split(Axis::V, kids)
    }

    /// Hidden `X` (V54: not focused, not visible) holding a tiled window per leaf.
    fn fake_with(leaves: &[u32]) -> FakeAero {
        let mut f = FakeAero::new(&desk3(), &[("X", 2, false, 0, Axis::H), ("T", 2, true, 0, Axis::H)], "T");
        for &id in leaves {
            f.add_window("X", id, "com.example.app", "", false);
        }
        f.add_window("X", 999, "com.example.app", "floating", true);
        f
    }

    fn run(f: &mut FakeAero, cmds: &[Cmd]) -> Result<(), String> {
        cmds.iter().try_for_each(|c| f.run(c).map_err(|e| format!("{c}: {e}")))
    }

    fn perms(xs: &[u32]) -> Vec<Vec<u32>> {
        if xs.len() <= 1 {
            return vec![xs.to_vec()];
        }
        (0..xs.len())
            .flat_map(|i| {
                let mut rest = xs.to_vec();
                let x = rest.remove(i);
                perms(&rest).into_iter().map(move |mut p| {
                    p.insert(0, x);
                    p
                })
            })
            .collect()
    }

    /// Start trees for one start order: flat, and nested two deep, both on the other root axis.
    fn starts(order: &[u32], want: Axis) -> Vec<Tree> {
        let w = |ids: &[u32]| ids.iter().map(|&i| Win(i)).collect::<Vec<_>>();
        let mut out = vec![Split(want.opposite(), w(order))];
        if order.len() >= 3 {
            let mut kids = vec![Split(want, w(&order[..2]))];
            kids.extend(w(&order[2..]));
            out.push(Split(want.opposite(), kids));
        }
        out
    }

    /// The trees T31 proved live, the motivating case, and sub-containers placed first and in
    /// the middle of their parent (T31 only had them last).
    fn targets() -> Vec<(Tree, usize)> {
        vec![
            (h(vec![Win(44), v(vec![Win(10), Win(49)])]), 8),
            (v(vec![Win(5), h(vec![Win(3), Win(8), Win(1)])]), 14),
            (h(vec![Win(1), v(vec![Win(2), h(vec![Win(3), Win(4)])]), Win(5)]), 22),
            (v(vec![h(vec![Win(6), Win(5), Win(4)]), h(vec![Win(3), v(vec![Win(2), Win(1)])])]), 33),
            (h(vec![v(vec![h(vec![Win(1), Win(2)]), Win(3)]), Win(4)]), 3 + 9 + 2 + 1),
            (h(vec![Win(1), v(vec![Win(2), h(vec![Win(3), Win(4)]), Win(5)])]), 3 + 16 + 3 + 1),
        ]
    }

    /// V54: every command names a window id or the workspace, so focus is never needed.
    fn targets_by_id(cmds: &[Cmd]) -> bool {
        cmds.iter().all(|c| match c {
            Cmd::MoveWindow { .. } | Cmd::JoinWith { .. } => true,
            Cmd::Flatten { ws } | Cmd::LayoutKind { ws, .. } | Cmd::Layout { ws, .. } => ws == "X",
            _ => false,
        })
    }

    #[test]
    fn plan_count_matches_the_t31_formula() {
        for (tree, n) in targets() {
            let cmds = plan("X", &tree);
            assert_eq!(cmds.len(), n, "{tree:?}");
            assert!(targets_by_id(&cmds));
            assert_eq!(cmds[..3], [Cmd::Flatten { ws: "X".into() }, Cmd::LayoutKind { ws: "X".into(), layout: Kind::Tiles }, Cmd::Layout {
                ws: "X".into(),
                axis: match &tree {
                    Split(a, _) => *a,
                    Win(_) => unreachable!(),
                }
            }]);
        }
    }

    #[test]
    fn empty_workspace_sends_nothing_and_one_window_only_sets_the_root() {
        assert!(plan("X", &h(vec![])).is_empty());
        let t = tpl("h", r#"[{ app = "a" }]"#);
        assert!(rebuild(&t, &[]).is_empty());
        assert!(rebuild(&t, &[win(3, "a", "", "floating")]).is_empty(), "floating is not counted");
        assert_eq!(plan("X", &v(vec![Win(7)])), [
            Cmd::Flatten { ws: "X".into() },
            Cmd::LayoutKind { ws: "X".into(), layout: Kind::Tiles },
            Cmd::Layout { ws: "X".into(), axis: Axis::V }
        ]);
    }

    #[test]
    fn every_start_order_and_most_recent_window_builds_the_canonical_tree_and_twice_is_the_same() {
        for (want, _) in targets() {
            let Split(axis, _) = want else { unreachable!() };
            let leaves = want.leaves();
            let cmds = plan("X", &want);
            for order in perms(&leaves) {
                for start in starts(&order, axis) {
                    for &recent in &leaves {
                        let mut f = fake_with(&leaves);
                        f.set_tree("X", &start, recent);
                        run(&mut f, &cmds).unwrap_or_else(|e| panic!("{want:?} from {start:?}: {e}"));
                        assert_eq!(f.tree("X"), want, "from {start:?}, most recent {recent}");
                        run(&mut f, &cmds).unwrap();
                        assert_eq!(f.tree("X"), want, "twice, from {start:?}");
                        assert_eq!(f.focused, "T", "focus does not move");
                        assert!(f.win(999).is_some_and(|w| w.floating), "a floating window is left alone");
                    }
                }
            }
        }
    }

    #[test]
    fn rebuild_uses_the_promoted_root_axis() {
        let t = tpl("h", r#"[{ app = "a" }, [{ app = "b" }, [{ app = "c" }, { app = "d" }]]]"#);
        let ws = [tiled(2, "b"), tiled(3, "c"), tiled(4, "d")];
        let cmds = rebuild(&t, &ws);
        assert_eq!(cmds[2], Cmd::Layout { ws: "X".into(), axis: Axis::V });
        let mut f = fake_with(&[2, 3, 4]);
        run(&mut f, &cmds).unwrap();
        assert_eq!(f.tree("X"), v(vec![Win(2), h(vec![Win(3), Win(4)])]));
    }

    #[test]
    fn motivating_case_from_its_template() {
        let t = tpl("h", r#"[{ app = "line" }, [{ app = "igcl" }, { app = "msgr" }]]"#);
        let ws = [tiled(49, "msgr"), tiled(10, "igcl"), tiled(44, "line")];
        let mut f = fake_with(&[49, 10, 44]);
        run(&mut f, &rebuild(&t, &ws)).unwrap();
        assert_eq!(f.tree("X"), h(vec![Win(44), v(vec![Win(10), Win(49)])]));
    }

    /// Negative control 1 (from source, T31): join from the first leaf toward pos also wraps
    /// both leaves, but leaves the first one most recent, so the next move lands in the wrong slot.
    #[test]
    fn joining_toward_pos_puts_the_next_leaf_in_the_wrong_slot() {
        let want = h(vec![Win(1), v(vec![Win(2), Win(3), Win(4)])]);
        let cmds: Vec<Cmd> = plan("X", &want)
            .into_iter()
            .map(|c| match c {
                Cmd::JoinWith { id: 3, dir: Dir::Left } => Cmd::JoinWith { id: 2, dir: Dir::Right },
                c => c,
            })
            .collect();
        let mut f = fake_with(&[1, 2, 3, 4]);
        run(&mut f, &cmds).unwrap();
        assert_eq!(f.tree("X"), h(vec![Win(1), v(vec![Win(2), Win(4), Win(3)])]));
    }

    /// Negative control 2: building a sub-container before its parent has all its leaves makes
    /// the sub-container the parent's most recent child, and its axis is the move's axis, so
    /// deep move-in drops the next leaf at its index 0.
    #[test]
    fn building_a_sub_container_early_swallows_the_next_leaf() {
        let want = h(vec![Win(1), v(vec![Win(2), h(vec![Win(3), Win(4)]), Win(5)])]);
        let good = plan("X", &want);
        let ordered = 3 + 16;
        let mut early = good[..ordered].to_vec();
        early.extend([
            Cmd::JoinWith { id: 3, dir: Dir::Left },
            Cmd::MoveWindow { id: 4, dir: Dir::Left },
            Cmd::JoinWith { id: 4, dir: Dir::Up },
            Cmd::MoveWindow { id: 5, dir: Dir::Left },
        ]);
        let mut f = fake_with(&[1, 2, 3, 4, 5]);
        run(&mut f, &early).unwrap();
        assert_eq!(f.tree("X"), h(vec![Win(1), v(vec![Win(2), h(vec![Win(5), Win(3), Win(4)])])]));
        assert_eq!(good[ordered..], [
            Cmd::JoinWith { id: 3, dir: Dir::Left },
            Cmd::MoveWindow { id: 4, dir: Dir::Left },
            Cmd::MoveWindow { id: 5, dir: Dir::Left },
            Cmd::JoinWith { id: 4, dir: Dir::Up },
        ]);
    }

    #[test]
    fn fake_refuses_what_a_rebuild_never_sends() {
        let mut f = fake_with(&[1, 2, 3]);
        f.set_tree("X", &h(vec![Win(1), v(vec![Win(2), Win(3)])]), 3);
        // Move-out of a container (at its edge, or across its axis) and a floating window.
        assert!(f.run(&Cmd::MoveWindow { id: 3, dir: Dir::Down }).is_err());
        assert!(f.run(&Cmd::MoveWindow { id: 2, dir: Dir::Left }).is_err());
        assert!(f.run(&Cmd::MoveWindow { id: 999, dir: Dir::Left }).is_err());
        // At the root's edge `stop` is a no-op success.
        let mut f = fake_with(&[1, 2]);
        f.run(&Cmd::MoveWindow { id: 2, dir: Dir::Right }).unwrap();
        assert_eq!(f.tree("X"), h(vec![Win(1), Win(2)]));
        // A root whose only child is a container: normalization would change it.
        assert!(f.run(&Cmd::JoinWith { id: 2, dir: Dir::Left }).is_err());
    }

    #[test]
    fn no_blocks_no_templates() {
        assert!(effective(&parse("").unwrap().config, None).is_empty());
    }

    // --- V41 record (T35) ---

    const C: &str = r#"[{ app = "line" }, [{ app = "igcl" }, { app = "msgr", title = "^Messenger" }], { app = "none" }]"#;

    #[test]
    fn wanted_keeps_one_list_per_slot_path_empty_slots_included() {
        let t = tpl("h", C);
        let ws = [tiled(49, "msgr"), tiled(10, "igcl"), tiled(44, "line"), tiled(7, "kitty"), win(8, "msgr", "Messenger", "floating")];
        let mut r = wanted(&t, &ws);
        assert_eq!(r.fingerprint, r#"h[line, v[igcl, msgr title "^Messenger"], none]"#);
        let slots: Vec<(&str, Vec<u32>)> = r.slots.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        // msgr 49 has an empty title, so its slot misses it: it is a rest window.
        assert_eq!(slots, [("1", vec![44]), ("2.1", vec![10]), ("2.2", vec![]), ("3", vec![])]);
        assert_eq!(r.rest, [7, 49]);
        r = wanted(&t, &[]);
        assert_eq!(r.slots.len(), 4, "an empty workspace still names every slot");
    }

    #[test]
    fn fingerprint_sees_the_tree_not_when_or_source() {
        let t = tpl("h", C);
        let moved = Template { when: Some(Pred { builtin: Some(true), ..Pred::default() }), source: "profile desk3 #1".into(), ..t.clone() };
        assert_eq!(fingerprint(&moved), fingerprint(&t));
        for other in [tpl("v", C), tpl("h", r#"[{ app = "line" }, [{ app = "igcl" }, { app = "msgr" }], { app = "none" }]"#), tpl("h", r#"[{ app = "line" }, { app = "igcl" }]"#)] {
            assert_ne!(fingerprint(&other), fingerprint(&t));
        }
    }

    #[test]
    fn pass_compares_the_record_with_the_wanted_one() {
        let t = tpl("h", C);
        let ws = [tiled(44, "line"), tiled(10, "igcl")];
        let want = wanted(&t, &ws);
        assert_eq!(pending(None, &want), Some(Pending::FirstRun));
        assert_eq!(pending(Some(&want), &want), None);
        let arrived = wanted(&t, &[tiled(44, "line"), tiled(10, "igcl"), tiled(12, "igcl")]);
        assert_eq!(pending(Some(&want), &arrived), Some(Pending::WindowsChanged));
        let edited = wanted(&tpl("v", C), &ws);
        assert_eq!(pending(Some(&want), &edited), Some(Pending::TemplateChanged));
        assert_eq!(Pending::TemplateChanged.to_string(), "pending: template changed");
    }

    fn rec(fp: &str) -> TemplateRecord {
        TemplateRecord { fingerprint: fp.into(), slots: BTreeMap::new(), rest: vec![] }
    }

    /// Runs `execute` for `X` over three commands; `stop_at` = (index, fails?) aborts or fails there.
    fn exec(stop_at: Option<(usize, bool)>) -> (Result<bool>, TemplateRecords, Vec<Option<String>>, usize) {
        let mut records = TemplateRecords::from([("X".into(), rec("old")), ("Y".into(), rec("y"))]);
        let cmds = vec![Cmd::Flatten { ws: "X".into() }; 3];
        let mut saves = vec![];
        let mut ran = 0;
        let out = execute(
            &mut records,
            "X",
            rec("new"),
            &cmds,
            &mut |r| saves.push(r.get("X").map(|x| x.fingerprint.clone())),
            |_| match stop_at {
                Some((i, fail)) if i == ran => if fail { anyhow::bail!("refused") } else { Ok(false) },
                _ => {
                    ran += 1;
                    Ok(true)
                }
            },
        );
        (out, records, saves, ran)
    }

    #[test]
    fn record_is_removed_before_the_first_command_and_saved_after_the_last() {
        let (out, records, saves, ran) = exec(None);
        assert!(out.unwrap());
        assert_eq!((ran, saves), (3, vec![None, Some("new".into())]));
        assert_eq!(records["X"].fingerprint, "new");
        assert_eq!(records["Y"].fingerprint, "y", "other workspaces keep theirs");
    }

    #[test]
    fn a_rebuild_that_fails_or_aborts_halfway_leaves_no_record() {
        for fail in [true, false] {
            let (out, records, saves, ran) = exec(Some((1, fail)));
            assert_eq!(out.is_err(), fail);
            assert_eq!(out.ok(), (!fail).then_some(false));
            assert_eq!((ran, saves), (1, vec![None]), "saved without a record before the first command, never after");
            assert!(!records.contains_key("X") && records.contains_key("Y"));
        }
    }

    #[test]
    fn an_empty_workspace_gets_no_commands_and_no_record() {
        let t = tpl("h", C);
        assert!(rebuild(&t, &[]).is_empty());
        let mut records = TemplateRecords::from([("X".into(), rec("old"))]);
        let mut saves = 0;
        assert!(execute(&mut records, "X", wanted(&t, &[]), &[], &mut |_| saves += 1, |_| unreachable!()).unwrap());
        assert!(records.is_empty());
        assert_eq!(saves, 1);
    }
}
