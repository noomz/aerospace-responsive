//! Test fixtures mirroring the user's real desk.

use crate::display::{Display, Rect, Screen};

fn screen(cg_id: u32, name: &str, bounds: Rect, px: (u64, u64), rotation: f64, main: bool, builtin: bool, notch: f64) -> Screen {
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
        notch,
        menu_bar_height: 33.0,
    }
}

/// This machine's built-in panel as AppKit reported it: 1728x1117 pt with a 32 pt notch.
pub const BUILTIN_NOTCH: f64 = 32.0;

pub fn builtin(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "Built-in Retina Display".into(),
        screen: screen(1, "Built-in Retina Display", Rect { x: 0.0, y: 0.0, w: 1728.0, h: 1117.0 }, (3456, 2234), 0.0, true, true, BUILTIN_NOTCH),
    }
}

pub fn len_portrait(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "LEN T23i-10 (2)".into(),
        screen: screen(5, "LEN T23i-10", Rect { x: -1080.0, y: -803.0, w: 1080.0, h: 1920.0 }, (1080, 1920), 270.0, false, false, 0.0),
    }
}

pub fn len_landscape(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "LEN T23i-10 (1)".into(),
        screen: screen(6, "LEN T23i-10", Rect { x: 1728.0, y: 0.0, w: 1920.0, h: 1080.0 }, (1920, 1080), 0.0, false, false, 0.0),
    }
}

/// An external landscape MAIN display (office), 2560x1440 pt at 2x.
pub fn studio(aero_id: u32) -> Display {
    Display {
        aero_id,
        aero_name: "Studio Display".into(),
        screen: screen(7, "Studio Display", Rect { x: 1728.0, y: 0.0, w: 2560.0, h: 1440.0 }, (5120, 2880), 0.0, true, false, 0.0),
    }
}

/// portrait (monitor 1, leftmost) | built-in (2) | landscape (3), as attached today.
pub fn desk3() -> Vec<Display> {
    vec![len_portrait(1), builtin(2), len_landscape(3)]
}

use crate::aero::{Aero, Cmd, Dir, Sink, Subscription};
use crate::template::Tree;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::config::Axis;
use crate::world::{AeroMonitor, Snapshot, WinInfo, WindowRow, WorkspaceRow};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct FakeWin {
    pub id: u32,
    pub app: String,
    pub title: String,
    pub floating: bool,
}

#[derive(Debug, Clone)]
pub struct FakeWs {
    pub name: String,
    pub monitor: u32,
    pub visible: bool,
    pub windows: Vec<FakeWin>,
    pub root: Axis,
    /// `v_accordion` / `h_accordion` instead of tiles. `layout --root ... tiles|accordion` sets it.
    pub accordion: bool,
    /// The tiling tree once a test or a `move` / `join-with` needs one. `None` = the tiled
    /// windows sit flat under the root in `windows` order. Its root axis follows `root`.
    pub tree: Option<FakeSplit>,
}

/// A child in a container's most-recent list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Win(u32),
    Split(u32),
}

#[derive(Debug, Clone)]
pub enum FakeNode {
    Win(u32),
    Split(FakeSplit),
}

impl FakeNode {
    fn key(&self) -> Key {
        match self {
            FakeNode::Win(id) => Key::Win(*id),
            FakeNode::Split(s) => Key::Split(s.id),
        }
    }

    fn to_tree(&self) -> Tree {
        match self {
            FakeNode::Win(id) => Tree::Win(*id),
            FakeNode::Split(s) => s.to_tree(),
        }
    }
}

/// A tiles container with the fork's semantics for what a rebuild sends (TreeNode.swift,
/// MoveCommand.swift, JoinWithCommand.swift): `bind` raises the child and every ancestor to most
/// recent. Paths a rebuild never takes are errors, so a plan that strays onto them fails.
#[derive(Debug, Clone)]
pub struct FakeSplit {
    pub id: u32,
    pub axis: Axis,
    pub kids: Vec<FakeNode>,
    /// Most recent last.
    pub mru: Vec<Key>,
}

impl FakeSplit {
    /// Ids count up from `next`; the most-recent order is child order.
    fn from_tree(t: &Tree, next: &mut u32) -> FakeSplit {
        let Tree::Split(axis, kids) = t else { panic!("a container is a Split") };
        *next += 1;
        let id = *next;
        let kids: Vec<FakeNode> = kids
            .iter()
            .map(|k| match k {
                Tree::Win(w) => FakeNode::Win(*w),
                s => FakeNode::Split(FakeSplit::from_tree(s, next)),
            })
            .collect();
        FakeSplit { id, axis: *axis, mru: kids.iter().map(FakeNode::key).collect(), kids }
    }

    fn to_tree(&self) -> Tree {
        Tree::Split(self.axis, self.kids.iter().map(FakeNode::to_tree).collect())
    }

    fn find(&self, key: Key) -> Option<Vec<usize>> {
        if key == Key::Split(self.id) {
            return Some(vec![]);
        }
        self.kids.iter().enumerate().find_map(|(i, k)| match k {
            _ if k.key() == key => Some(vec![i]),
            FakeNode::Split(s) => s.find(key).map(|p| [vec![i], p].concat()),
            FakeNode::Win(_) => None,
        })
    }

    fn split(&self, path: &[usize]) -> &FakeSplit {
        path.iter().fold(self, |s, &i| match &s.kids[i] {
            FakeNode::Split(x) => x,
            FakeNode::Win(_) => panic!("path ends at a window"),
        })
    }

    fn split_mut(&mut self, path: &[usize]) -> &mut FakeSplit {
        let mut s = self;
        for &i in path {
            s = match &mut s.kids[i] {
                FakeNode::Split(x) => x,
                FakeNode::Win(_) => panic!("path ends at a window"),
            };
        }
        s
    }

    fn max_id(&self) -> u32 {
        self.kids.iter().filter_map(|k| if let FakeNode::Split(s) = k { Some(s.max_id()) } else { None }).fold(self.id, u32::max)
    }

    fn unbind(&mut self, key: Key) -> FakeNode {
        let path = self.find(key).expect("unbind: not bound");
        let (&i, parent) = path.split_last().expect("unbind: the root");
        let p = self.split_mut(parent);
        p.mru.retain(|k| *k != key);
        p.kids.remove(i)
    }

    /// `bind`: insert into container `parent` at `index` (`None` = last), then mark most recent.
    fn bind(&mut self, parent: Key, node: FakeNode, index: Option<usize>) {
        let mut path = self.find(parent).expect("bind: no such container");
        let p = self.split_mut(&path);
        let index = index.unwrap_or(p.kids.len());
        p.kids.insert(index, node);
        path.push(index);
        self.raise(&path);
    }

    /// `markAsMostRecentChild`: the node at `path` and every ancestor.
    fn raise(&mut self, path: &[usize]) {
        for d in (0..path.len()).rev() {
            let p = self.split_mut(&path[..d]);
            let k = p.kids[path[d]].key();
            p.mru.retain(|x| *x != k);
            p.mru.push(k);
        }
    }

    fn most_recent(&self) -> Key {
        self.mru.last().copied().unwrap_or_else(|| self.kids.last().expect("inhabited").key())
    }

    /// The sibling of child `i` toward `dir`, if this container runs along `dir`.
    fn neighbour(&self, i: usize, dir: Dir) -> Option<usize> {
        if self.axis != dir.axis() {
            return None;
        }
        if dir.is_pos() { (i + 1 < self.kids.len()).then_some(i + 1) } else { i.checked_sub(1) }
    }

    /// `move --window-id id --boundaries workspace --boundaries-action stop dir`.
    fn move_window(&mut self, id: u32, dir: Dir) -> Result<(), String> {
        let me = Key::Win(id);
        let path = self.find(me).ok_or(format!("no window {id} in the tree"))?;
        let (&i, pp) = path.split_last().expect("a window is not the root");
        let parent = self.split(pp);
        let Some(j) = parent.neighbour(i, dir) else {
            // moveOut: from the root it reaches the workspace boundary, and `stop` succeeds.
            return if pp.is_empty() { Ok(()) } else { Err(format!("fake: move-out of a container is not modeled ({id} {})", dir.as_str())) };
        };
        let (into, at) = match &parent.kids[j] {
            FakeNode::Win(_) => (Key::Split(parent.id), Some(j)), // swap
            FakeNode::Split(s) => {
                // deepMoveIn: a container along the move takes the window first; otherwise
                // follow most-recent children down to a window and land right after it.
                let mut s = s;
                loop {
                    if s.axis == dir.axis() {
                        break (Key::Split(s.id), Some(0));
                    }
                    let k = s.most_recent();
                    let at = s.kids.iter().position(|c| c.key() == k).expect("most recent is a child");
                    match &s.kids[at] {
                        FakeNode::Split(c) => s = c,
                        FakeNode::Win(_) => break (Key::Split(s.id), Some(at + 1)),
                    }
                }
            }
        };
        let w = self.unbind(me);
        self.bind(into, w, at);
        Ok(())
    }

    /// `join-with --window-id id dir`: the closest ancestor along `dir` with a neighbour there;
    /// the neighbour and the window go into a new container of the opposite axis at the
    /// neighbour's place, the neighbour first, the window first for a positive `dir`, else last.
    fn join_with(&mut self, id: u32, dir: Dir) -> Result<(), String> {
        let me = Key::Win(id);
        let mut path = self.find(me).ok_or(format!("no window {id} in the tree"))?;
        let (parent, nb) = loop {
            let Some((&i, pp)) = path.split_last() else { return Err("No windows in the specified direction".into()) };
            let p = self.split(pp);
            if let Some(nb) = p.neighbour(i, dir) {
                break (p, nb);
            }
            path.pop();
        };
        let (pkey, axis, target) = (Key::Split(parent.id), parent.axis.opposite(), parent.kids[nb].key());
        let target = self.unbind(target);
        let k = self.max_id() + 1;
        self.bind(pkey, FakeNode::Split(FakeSplit { id: k, axis, kids: vec![], mru: vec![] }), Some(nb));
        let w = self.unbind(me);
        self.bind(Key::Split(k), target, Some(0));
        self.bind(Key::Split(k), w, if dir.is_pos() { Some(0) } else { None });
        Ok(())
    }

    /// `flatten-workspace-tree`: every leaf, depth-first, bound last to the root.
    fn flatten(&mut self) {
        let leaves = self.to_tree().leaves();
        self.kids.clear();
        self.mru.clear();
        for id in leaves {
            self.bind(Key::Split(self.id), FakeNode::Win(id), None);
        }
    }

    /// V43: neither normalization setting may find anything to change between commands: no
    /// container with fewer than two children, none of its parent's axis, and no root whose only
    /// child is a container.
    fn check_normal(&self) -> Result<(), String> {
        fn below(s: &FakeSplit) -> Result<(), String> {
            for k in &s.kids {
                if let FakeNode::Split(c) = k {
                    if c.kids.len() < 2 || c.axis == s.axis {
                        return Err(format!("fake: normalization would change {:?}", c.to_tree()));
                    }
                    below(c)?;
                }
            }
            Ok(())
        }
        if let [FakeNode::Split(_)] = self.kids.as_slice() {
            return Err(format!("fake: normalization would change root {:?}", self.to_tree()));
        }
        below(self)
    }

    /// Follow the workspace: root axis, windows that left or stopped tiling, new tiled windows.
    fn sync(&mut self, axis: Axis, tiled: &[u32]) {
        fn prune(s: &mut FakeSplit) {
            s.kids.retain_mut(|k| match k {
                FakeNode::Split(c) => {
                    prune(c);
                    !c.kids.is_empty()
                }
                FakeNode::Win(_) => true,
            });
            let keys: Vec<Key> = s.kids.iter().map(FakeNode::key).collect();
            s.mru.retain(|k| keys.contains(k));
        }
        self.axis = axis;
        for id in self.to_tree().leaves() {
            if !tiled.contains(&id) {
                self.unbind(Key::Win(id));
            }
        }
        prune(self);
        for &id in tiled {
            if self.find(Key::Win(id)).is_none() {
                self.bind(Key::Split(self.id), FakeNode::Win(id), None);
            }
        }
    }
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
    /// On the next `layout --root`, AeroSpace moves this workspace to this monitor by itself.
    pub drift_on_layout: Option<(String, u32)>,
    /// When a window on this workspace floats, its root turns accordion (AeroSpace promotes a
    /// lone accordion child container once its tiled sibling leaves).
    pub accordion_on_float: Option<String>,
    /// Make `windows()` fail, as a socket blip does mid-pass.
    pub fail_windows: bool,
    /// One flag per `subscribe` call: true while that Subscription is alive.
    pub subs: Vec<Arc<AtomicBool>>,
    /// Make `subscribe` fail to spawn.
    pub fail_subscribe: bool,
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
                            FakeWin { id: next_id, app: "com.example.app".into(), title: String::new(), floating: false }
                        })
                        .collect(),
                    root: *root,
                    accordion: false,
                    tree: None,
                })
                .collect(),
            focused: focused.into(),
            log: vec![],
            refuse: BTreeSet::new(),
            ignore_layout: false,
            drift_on_layout: None,
            accordion_on_float: None,
            fail_windows: false,
            subs: vec![],
            fail_subscribe: false,
        }
    }

    pub fn ws(&self, name: &str) -> &FakeWs {
        self.ws.iter().find(|w| w.name == name).expect("no such fake workspace")
    }

    /// Subscriptions still alive (spawned and not dropped).
    pub fn live_subs(&self) -> usize {
        self.subs.iter().filter(|a| a.load(Ordering::SeqCst)).count()
    }

    pub fn set_floating(&mut self, id: u32, floating: bool) {
        if let Ok(w) = self.win_mut(id) {
            w.floating = floating;
        }
    }

    pub fn add_window(&mut self, ws: &str, id: u32, app: &str, title: &str, floating: bool) {
        let i = self.idx(ws).expect("no such fake workspace");
        self.ws[i].windows.push(FakeWin { id, app: app.into(), title: title.into(), floating });
    }

    pub fn win(&self, id: u32) -> Option<&FakeWin> {
        self.ws.iter().flat_map(|w| &w.windows).find(|w| w.id == id)
    }

    fn win_mut(&mut self, id: u32) -> Result<&mut FakeWin, String> {
        self.ws.iter_mut().flat_map(|w| &mut w.windows).find(|w| w.id == id).ok_or_else(|| format!("no window {id}"))
    }

    /// `move-node-to-workspace --window-id`: the window keeps its floating state.
    pub fn move_window(&mut self, id: u32, to: &str) {
        let w = self.close_window(id).expect("no such fake window");
        let i = self.idx(to).expect("no such fake workspace");
        self.ws[i].windows.push(w);
    }

    pub fn close_window(&mut self, id: u32) -> Option<FakeWin> {
        let ws = self.ws.iter_mut().find(|w| w.windows.iter().any(|x| x.id == id))?;
        let i = ws.windows.iter().position(|x| x.id == id)?;
        Some(ws.windows.remove(i))
    }

    /// The workspace rows, without borrowing `self` mutably.
    pub fn clone_rows(&self) -> Vec<WorkspaceRow> {
        self.ws
            .iter()
            .map(|w| WorkspaceRow { workspace: w.name.clone(), monitor: w.monitor, visible: w.visible, focused: w.name == self.focused })
            .collect()
    }

    fn tiled_ids(&self, i: usize) -> Vec<u32> {
        self.ws[i].windows.iter().filter(|w| !w.floating).map(|w| w.id).collect()
    }

    /// Workspace `ws` gets this tiling tree (and its root axis), with `most_recent` raised the
    /// way AeroSpace raises the window it last bound or focused.
    pub fn set_tree(&mut self, ws: &str, tree: &Tree, most_recent: u32) {
        let i = self.idx(ws).expect("no such fake workspace");
        let tiled = self.tiled_ids(i);
        assert!(tree.leaves().iter().all(|id| tiled.contains(id)), "every leaf must be a tiled window on {ws}");
        let mut t = FakeSplit::from_tree(tree, &mut 0);
        let path = t.find(Key::Win(most_recent)).expect("most_recent must be a leaf");
        t.raise(&path);
        self.ws[i].root = t.axis;
        self.ws[i].tree = Some(t);
    }

    /// The tiling tree of `ws` as the daemon would want to see it (it never can, C20).
    pub fn tree(&self, ws: &str) -> Tree {
        let i = self.idx(ws).expect("no such fake workspace");
        let tiled = self.tiled_ids(i);
        let mut t = self.ws[i].tree.clone().unwrap_or(FakeSplit { id: 0, axis: self.ws[i].root, kids: vec![], mru: vec![] });
        t.sync(self.ws[i].root, &tiled);
        t.to_tree()
    }

    fn tree_mut(&mut self, i: usize) -> &mut FakeSplit {
        let (tiled, axis) = (self.tiled_ids(i), self.ws[i].root);
        let t = self.ws[i].tree.get_or_insert_with(|| FakeSplit { id: 0, axis, kids: vec![], mru: vec![] });
        t.sync(axis, &tiled);
        t
    }

    /// The workspace holding tiled window `id`; a rebuild never moves a floating one (C23).
    fn tiled_ws(&self, id: u32) -> Result<usize, String> {
        let i = self.ws.iter().position(|w| w.windows.iter().any(|x| x.id == id)).ok_or(format!("no window {id}"))?;
        if self.win(id).is_some_and(|w| w.floating) {
            return Err("moving floating windows isn't yet supported".into());
        }
        Ok(i)
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
            let kind = if w.accordion { "accordion" } else { "tiles" };
            let layout = if w.root == Axis::H { format!("h_{kind}") } else { format!("v_{kind}") };
            for x in &w.windows {
                windows.push(WindowRow { id: x.id, workspace: w.name.clone(), monitor: w.monitor, root_layout: layout.clone() });
            }
        }
        Ok(Snapshot { windows })
    }

    fn workspaces(&mut self) -> anyhow::Result<Vec<WorkspaceRow>> {
        Ok(self.clone_rows())
    }

    fn window_details(&mut self) -> anyhow::Result<Vec<WinInfo>> {
        if self.fail_windows {
            anyhow::bail!("list-windows: connection refused");
        }
        let mut out = vec![];
        for w in &self.ws {
            let tiled = if w.root == Axis::H { "h_tiles" } else { "v_tiles" };
            for x in &w.windows {
                let layout = if x.floating { "floating" } else { tiled };
                let app_name = x.app.rsplit('.').next().unwrap_or_default().to_string();
                out.push(WinInfo { id: x.id, workspace: w.name.clone(), app: x.app.clone(), app_name, title: x.title.clone(), layout: layout.into() });
            }
        }
        Ok(out)
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
            Cmd::Flatten { ws } => {
                let i = self.idx(ws)?;
                if self.ws[i].tree.is_some() {
                    self.tree_mut(i).flatten();
                }
                Ok(())
            }
            Cmd::MoveWindow { id, dir } => {
                let i = self.tiled_ws(*id)?;
                let t = self.tree_mut(i);
                t.move_window(*id, *dir)?;
                t.check_normal()
            }
            Cmd::JoinWith { id, dir } => {
                let i = self.tiled_ws(*id)?;
                let t = self.tree_mut(i);
                t.join_with(*id, *dir)?;
                t.check_normal()
            }
            Cmd::LayoutKind { ws, layout } => {
                let i = self.idx(ws)?;
                if !self.ignore_layout {
                    self.ws[i].accordion = *layout == crate::config::Kind::Accordion;
                }
                Ok(())
            }
            Cmd::Layout { ws, axis } => {
                if let Some((name, to)) = self.drift_on_layout.take() {
                    let j = self.idx(&name)?;
                    self.ws[j].monitor = to;
                }
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
            Cmd::Float { id } => {
                if let Some(ws) = self.accordion_on_float.take() {
                    let i = self.idx(&ws)?;
                    self.ws[i].accordion = true;
                }
                self.win_mut(*id).map(|w| w.floating = true)
            }
            Cmd::Tile { id } => self.win_mut(*id).map(|w| w.floating = false),
            Cmd::ReloadConfig => Ok(()),
        }
    }

    /// Tests feed `Msg::Window` / `Msg::SubscribeExit` themselves; the sink is unused.
    fn subscribe(&mut self, _sink: Sink) -> anyhow::Result<Subscription> {
        if self.fail_subscribe {
            anyhow::bail!("cannot spawn `aerospace subscribe`");
        }
        let alive = Arc::new(AtomicBool::new(true));
        self.subs.push(alive.clone());
        Ok(Subscription::new(move || alive.store(false, Ordering::SeqCst)))
    }
}
