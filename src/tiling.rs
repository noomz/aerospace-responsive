//! Exclusive tiling (SPEC §G): on a named workspace only windows its tiling allowlist admits,
//! within each entry's tile limit, tile; every other window arriving tiled gets the else-action.
//! Pure: the worker feeds it window reads and executes what it returns.

use crate::aero::Cmd;
use crate::config::{Config, ElseAction, Entry, Profile, TilingBlock};
use crate::state::{Floated, RuleRecord, TilingMemory};
use crate::world::WinInfo;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

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

/// Why the rule floats a window (the closed set `explain` prints). Entries are 1-based.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Why {
    NotAllowlisted,
    Full { entry: usize },
    CarveOut { entry: usize },
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::NotAllowlisted => write!(f, "not allowlisted"),
            Why::Full { entry } => write!(f, "entry {entry} full"),
            Why::CarveOut { entry } => write!(f, "entry {entry} carve-out"),
        }
    }
}

/// The entry that governs `w`: the first it matches now, by current title (V2, V4).
pub fn first_match(rule: &Rule, w: &WinInfo) -> Option<usize> {
    rule.tiling_allowlist.iter().position(|e| e.app == w.app && e.title.as_ref().is_none_or(|t| t.is_match(&w.title)))
}

/// Entry `i`'s windows tiled on the rule's workspace now, whoever tiled them, `except` one (V3).
pub fn tiled_in(rule: &Rule, i: usize, windows: &[WinInfo], except: u32) -> usize {
    windows.iter().filter(|w| w.workspace == rule.workspace && w.id != except && w.tiled() && first_match(rule, w) == Some(i)).count()
}

/// Would `w` tile here if it arrived tiled now? `Ok(entry index)` or why not (V2, V3).
pub fn admit(rule: &Rule, w: &WinInfo, windows: &[WinInfo]) -> Result<usize, Why> {
    let i = first_match(rule, w).ok_or(Why::NotAllowlisted)?;
    match rule.tiling_allowlist[i].max_tiled {
        Some(0) => Err(Why::CarveOut { entry: i + 1 }),
        Some(n) if tiled_in(rule, i, windows, w.id) >= n as usize => Err(Why::Full { entry: i + 1 }),
        _ => Ok(i),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// Arrived not tiled (dialog, sticky, floated by a callback or by you): never touched (V1).
    Ignore,
    Admit,
    /// Else-action; the window becomes rule-floated.
    Float(Why),
}

/// V1, V2: the rule acts only on a window arriving tiled. `windows` = the whole read.
pub fn arrival(rule: &Rule, w: &WinInfo, windows: &[WinInfo]) -> Arrival {
    if !w.tiled() {
        return Arrival::Ignore;
    }
    match admit(rule, w, windows) {
        Ok(_) => Arrival::Admit,
        Err(why) => Arrival::Float(why),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leave {
    /// You changed its state since the rule floated it (tiled it, made it sticky, ...): the
    /// rule's float is gone, so is its memory (V6).
    Drop,
    Retile,
    /// Still floating, now rule-floated on its destination.
    StayFloated(Why),
}

/// V5: a rule-floated window left; its destination (`w.workspace`, rule `dest`) decides.
pub fn leave(w: &WinInfo, dest: Option<&Rule>, windows: &[WinInfo]) -> Leave {
    if !w.floating() {
        return Leave::Drop;
    }
    match dest.map(|r| admit(r, w, windows)) {
        None | Some(Ok(_)) => Leave::Retile,
        Some(Err(why)) => Leave::StayFloated(why),
    }
}

/// Why a workspace converges (V7). `Changed` carries `field (source)` strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Reason {
    FirstRun,
    Changed { fields: Vec<String> },
    Removed,
}

/// V7, V8: converge only when the record is missing, differs, or has lost its rule.
pub fn pending(rule: Option<&Rule>, record: Option<&RuleRecord>) -> Option<Reason> {
    match (rule, record) {
        (Some(_), None) => Some(Reason::FirstRun),
        (Some(r), Some(rec)) => {
            let changed = rec.changed(r);
            (!changed.is_empty()).then(|| Reason::Changed {
                fields: changed
                    .into_iter()
                    .map(|f| format!("{f} ({})", if f == "else-action" { &r.else_action_source } else { &r.tiling_allowlist_source }))
                    .collect(),
            })
        }
        (None, Some(_)) => Some(Reason::Removed),
        (None, None) => None,
    }
}

/// One workspace's converge: commands in order, then the memory and record it leaves.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub workspace: String,
    pub reason: Reason,
    /// Each command with the window's bundle id, so memory can follow command by command.
    pub steps: Vec<(Cmd, String)>,
    pub floated_after: BTreeSet<Floated>,
    pub record_after: Option<RuleRecord>,
}

impl Plan {
    /// `rule changed: tiling-allowlist (profile desk3 #1)`, `first run`, `rule removed, re-tiles 2`.
    pub fn describe(&self) -> String {
        self.reason.describe(self.steps.len())
    }
}

/// Every workspace with a rule or a record whose converge is pending, in name order.
pub fn plan_converge(rules: &[Rule], mem: &TilingMemory, windows: &[WinInfo]) -> Vec<Plan> {
    let names: BTreeSet<&String> = rules.iter().map(|r| &r.workspace).chain(mem.records.keys()).collect();
    let empty = BTreeSet::new();
    let mut out = vec![];
    for ws in names {
        let rule = rules.iter().find(|r| &r.workspace == ws);
        let Some(reason) = pending(rule, mem.records.get(ws)) else { continue };
        let floated = mem.rule_floated.get(ws).unwrap_or(&empty);
        let was_floated = |w: &WinInfo| floated.contains(&Floated { window_id: w.id, app: w.app.clone() });
        let here = windows.iter().filter(|w| &w.workspace == ws);
        let mut plan = Plan { workspace: ws.clone(), reason, steps: vec![], floated_after: BTreeSet::new(), record_after: rule.map(RuleRecord::of) };
        match rule {
            // Removal: the rule's own floats re-tile, nothing else changes.
            None => {
                for w in here.filter(|w| w.floating() && was_floated(w)) {
                    plan.steps.push((Cmd::Tile { id: w.id }, w.app.clone()));
                }
            }
            // Every tiled or rule-floated window is an arrival; the lowest id keeps a slot.
            Some(r) => {
                let mut cands: Vec<&WinInfo> = here.filter(|w| w.tiled() || (w.floating() && was_floated(w))).collect();
                cands.sort_by_key(|w| w.id);
                let mut used = vec![0usize; r.tiling_allowlist.len()];
                for w in cands {
                    let tile = first_match(r, w).is_some_and(|i| {
                        let room = r.tiling_allowlist[i].max_tiled.is_none_or(|n| used[i] < n as usize);
                        used[i] += room as usize;
                        room
                    });
                    match (tile, w.tiled()) {
                        (true, false) => plan.steps.push((Cmd::Tile { id: w.id }, w.app.clone())),
                        (false, true) => plan.steps.push((Cmd::Float { id: w.id }, w.app.clone())),
                        _ => {}
                    }
                    if !tile {
                        plan.floated_after.insert(Floated { window_id: w.id, app: w.app.clone() });
                    }
                }
            }
        }
        out.push(plan);
    }
    out
}

/// C3 gates only rules that need events: while inert, a configured rule converges nothing, but a
/// removal (a record without a rule) still re-tiles the rule's own floats (V20).
pub fn runnable(plan: &Plan, live: bool) -> bool {
    live || plan.reason == Reason::Removed
}

/// Memory follows one executed converge command.
pub fn note(mem: &mut TilingMemory, ws: &str, cmd: &Cmd, app: &str) {
    match cmd {
        Cmd::Float { id } => mem.insert(ws, *id, app),
        Cmd::Tile { id } => {
            mem.remove(*id);
        }
        _ => {}
    }
}

/// A workspace's converge completed: its memory and record become the plan's (V8).
pub fn commit(mem: &mut TilingMemory, plan: &Plan) {
    for f in &plan.floated_after {
        mem.remove(f.window_id);
    }
    mem.rule_floated.remove(&plan.workspace);
    if !plan.floated_after.is_empty() {
        mem.rule_floated.insert(plan.workspace.clone(), plan.floated_after.clone());
    }
    match &plan.record_after {
        Some(r) => mem.records.insert(plan.workspace.clone(), r.clone()),
        None => mem.records.remove(&plan.workspace),
    };
}

impl Reason {
    /// `re_tiles` = the removal's re-tile count.
    pub fn describe(&self, re_tiles: usize) -> String {
        match self {
            Reason::FirstRun => "first run".into(),
            Reason::Changed { fields } => format!("rule changed: {}", fields.join(", ")),
            Reason::Removed => format!("rule removed, re-tiles {re_tiles}"),
        }
    }
}

/// One allowlist entry with how many of its windows tile on the workspace now.
#[derive(Debug, Clone, Serialize)]
pub struct EntryView {
    #[serde(flatten)]
    pub entry: Entry,
    /// `None` without a window read.
    pub tiled: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowView {
    pub id: u32,
    pub app: String,
    pub app_name: String,
    pub title: String,
    pub layout: String,
    pub verdict: String,
}

/// `explain`'s view of one ruled workspace (I.cli.explain).
#[derive(Debug, Clone, Serialize)]
pub struct WsView {
    pub workspace: String,
    /// `None` = the rule was removed and its record still waits for the converge.
    pub rule: Option<Rule>,
    pub entries: Vec<EntryView>,
    /// `up to date` | `pending: <reason>`.
    pub converge: String,
    /// `None` without a window read.
    pub windows: Option<Vec<WindowView>>,
}

/// Each window's verdict, from the closed set: `entry N (k/limit)`; `rule-floated: <why>`;
/// `yours (rule ignores)`; `tiled by you, not allowlisted | over entry N limit`.
fn verdict(rule: Option<&Rule>, w: &WinInfo, here: &[&WinInfo], wins: &[WinInfo], mem: &TilingMemory) -> String {
    let ours = mem.floated_on(w.id, &w.app) == Some(w.workspace.as_str());
    let Some(rule) = rule else {
        return if ours { "rule-floated: rule removed, re-tiles".into() } else { "yours (rule ignores)".into() };
    };
    if w.tiled() {
        let Some(i) = first_match(rule, w) else { return "tiled by you, not allowlisted".into() };
        let mut of_entry: Vec<u32> = here.iter().filter(|x| x.tiled() && first_match(rule, x) == Some(i)).map(|x| x.id).collect();
        of_entry.sort_unstable();
        let rank = of_entry.iter().position(|id| *id == w.id).unwrap_or(0);
        return match rule.tiling_allowlist[i].max_tiled {
            Some(n) if rank >= n as usize => format!("over entry {} limit", i + 1),
            Some(n) => format!("entry {} ({}/{n})", i + 1, of_entry.len().min(n as usize)),
            None => format!("entry {} ({}/∞)", i + 1, of_entry.len()),
        };
    }
    if !ours {
        return "yours (rule ignores)".into();
    }
    match admit(rule, w, wins) {
        Err(why) => format!("rule-floated: {why}"),
        Ok(i) => format!("rule-floated: entry {} (no promotion)", i + 1),
    }
}

/// `explain`'s exclusive-tiling section: every workspace with a rule or a record.
pub fn view(rules: &[Rule], mem: &TilingMemory, wins: Option<&[WinInfo]>) -> Vec<WsView> {
    let names: BTreeSet<&String> = rules.iter().map(|r| &r.workspace).chain(mem.records.keys()).collect();
    let plans = wins.map(|w| plan_converge(rules, mem, w)).unwrap_or_default();
    names
        .into_iter()
        .map(|ws| {
            let rule = rules.iter().find(|r| &r.workspace == ws);
            let here: Vec<&WinInfo> = wins.unwrap_or_default().iter().filter(|w| &w.workspace == ws).collect();
            let entries = rule.map_or(vec![], |r| {
                r.tiling_allowlist
                    .iter()
                    .enumerate()
                    .map(|(i, e)| EntryView { entry: e.clone(), tiled: wins.map(|all| tiled_in(r, i, all, u32::MAX)) })
                    .collect()
            });
            let converge = match pending(rule, mem.records.get(ws)) {
                None => "up to date".to_string(),
                Some(reason) => {
                    let re_tiles = match plans.iter().find(|p| &p.workspace == ws) {
                        Some(p) => p.steps.len(),
                        None => mem.rule_floated.get(ws).map_or(0, |s| s.len()),
                    };
                    format!("pending: {}", reason.describe(re_tiles))
                }
            };
            let windows = wins.map(|all| {
                let mut v: Vec<WindowView> = here
                    .iter()
                    .map(|w| WindowView {
                        id: w.id,
                        app: w.app.clone(),
                        app_name: w.app_name.clone(),
                        title: w.title.clone(),
                        layout: w.layout.clone(),
                        verdict: verdict(rule, w, &here, all, mem),
                    })
                    .collect();
                v.sort_by_key(|w| w.id);
                v
            });
            WsView { workspace: ws.clone(), rule: rule.cloned(), entries, converge, windows }
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

    const KITTY: &str = "net.kovidgoyal.kitty";

    /// kitty-on-T from I.cfg: kitty "Settings" carved out, one other kitty may tile.
    fn kitty_rule() -> Rule {
        let cfg = parse(
            "[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [\n\
             { app = \"net.kovidgoyal.kitty\", title = \"Settings\", max-tiled = 0 },\n\
             { app = \"net.kovidgoyal.kitty\", max-tiled = 1 },\n\
             { app = \"com.apple.TextEdit\" },\n]\n",
        )
        .unwrap()
        .config;
        effective(&cfg, None).remove(0)
    }

    fn win(id: u32, ws: &str, app: &str, title: &str, layout: &str) -> WinInfo {
        let app_name = app.rsplit('.').next().unwrap().to_string();
        WinInfo { id, workspace: ws.into(), app: app.into(), app_name, title: title.into(), layout: layout.into() }
    }

    #[test]
    fn arrival_matrix() {
        let r = kitty_rule();
        let tiled = |id, app: &str, title: &str| win(id, "T", app, title, "h_tiles");
        // Not allowlisted, arriving tiled: else-action.
        assert_eq!(arrival(&r, &tiled(1, "com.apple.Safari", "GitHub"), &[]), Arrival::Float(Why::NotAllowlisted));
        // Allowlisted with room: nothing.
        let k = tiled(2, KITTY, "noomz: ~");
        assert_eq!(arrival(&r, &k, std::slice::from_ref(&k)), Arrival::Admit);
        // Arriving floating, sticky or minimized: untouched, allowlisted or not (V1).
        for layout in ["floating", "sticky", "macos_native_minimized"] {
            assert_eq!(arrival(&r, &win(3, "T", "com.apple.Safari", "", layout), &[]), Arrival::Ignore);
            assert_eq!(arrival(&r, &win(4, "T", KITTY, "x", layout), &[]), Arrival::Ignore);
        }
    }

    #[test]
    fn first_match_governs_carve_out_and_limit() {
        let r = kitty_rule();
        let settings = win(5, "T", KITTY, "kitty SETTINGS", "h_tiles");
        assert_eq!(first_match(&r, &settings), Some(0), "title matches case-insensitively, first entry wins");
        assert_eq!(arrival(&r, &settings, &[]), Arrival::Float(Why::CarveOut { entry: 1 }));
        let a = win(6, "T", KITTY, "noomz: ~", "h_tiles");
        let b = win(7, "T", KITTY, "noomz: htop", "h_tiles");
        let all = [a.clone(), b.clone()];
        assert_eq!(arrival(&r, &b, &all), Arrival::Float(Why::Full { entry: 2 }), "a holds the one slot");
        // The limit counts tiled windows only, and on this workspace only.
        let gone = [win(6, "T", KITTY, "noomz: ~", "floating"), b.clone(), win(8, "X", KITTY, "", "h_tiles")];
        assert_eq!(arrival(&r, &b, &gone), Arrival::Admit);
        // Unlimited entry.
        let edits: Vec<WinInfo> = (10..14).map(|i| win(i, "T", "com.apple.TextEdit", "", "h_tiles")).collect();
        assert_eq!(arrival(&r, &edits[3], &edits), Arrival::Admit);
    }

    #[test]
    fn no_promotion_when_a_slot_frees() {
        // A rule-floated kitty stays floating when the tiled one closes: arrivals are the only
        // trigger, and a close is not an arrival. Converge is not run on a plain change.
        let r = kitty_rule();
        let mut mem = TilingMemory::default();
        mem.records.insert("T".into(), RuleRecord::of(&r));
        mem.insert("T", 7, KITTY);
        let after_close = [win(7, "T", KITTY, "noomz: htop", "floating")];
        assert!(plan_converge(std::slice::from_ref(&r), &mem, &after_close).is_empty());
    }

    #[test]
    fn leave_lets_the_destination_decide() {
        let r = kitty_rule();
        let x = Rule { workspace: "X".into(), ..r.clone() };
        let safari = win(1, "X", "com.apple.Safari", "", "floating");
        assert_eq!(leave(&win(1, "Y", "com.apple.Safari", "", "floating"), None, &[]), Leave::Retile, "no rule there");
        assert_eq!(leave(&safari, Some(&x), &[]), Leave::StayFloated(Why::NotAllowlisted));
        let k = win(2, "X", KITTY, "noomz: ~", "floating");
        assert_eq!(leave(&k, Some(&x), &[]), Leave::Retile, "admitted with room");
        let full = [k.clone(), win(3, "X", KITTY, "vim", "h_tiles")];
        assert_eq!(leave(&k, Some(&x), &full), Leave::StayFloated(Why::Full { entry: 2 }));
        // Tiled by you before it left: the rule's float is gone (V6).
        assert_eq!(leave(&win(2, "X", KITTY, "", "h_tiles"), Some(&x), &[]), Leave::Drop);
    }

    #[test]
    fn converge_first_run_lowest_id_keeps_the_slot() {
        let r = kitty_rule();
        let ws = [
            win(40, "T", KITTY, "b", "h_tiles"),
            win(30, "T", KITTY, "a", "h_tiles"),
            win(50, "T", "com.apple.Finder", "Downloads", "h_tiles"),
            win(60, "T", "com.apple.Preview", "x.pdf", "floating"),
            win(70, "X", "com.apple.Finder", "", "h_tiles"),
        ];
        let plans = plan_converge(std::slice::from_ref(&r), &TilingMemory::default(), &ws);
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert_eq!(p.reason, Reason::FirstRun);
        let cmds: Vec<&Cmd> = p.steps.iter().map(|(c, _)| c).collect();
        assert_eq!(cmds, [&Cmd::Float { id: 40 }, &Cmd::Float { id: 50 }], "30 keeps the kitty slot; your float 60 is ignored");
        let ids: Vec<u32> = p.floated_after.iter().map(|f| f.window_id).collect();
        assert_eq!(ids, [40, 50]);
        let mut mem = TilingMemory::default();
        commit(&mut mem, p);
        assert_eq!(mem.records.get("T"), Some(&RuleRecord::of(&r)));
        assert!(plan_converge(std::slice::from_ref(&r), &mem, &ws).is_empty(), "equal record: nothing");
    }

    #[test]
    fn converge_on_edit_retiles_admitted_and_floats_disallowed() {
        let old = kitty_rule();
        let mut mem = TilingMemory::default();
        mem.records.insert("T".into(), RuleRecord::of(&old));
        mem.insert("T", 20, "com.apple.Safari");
        // The edit admits Safari and drops TextEdit.
        let cfg = parse(
            "[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [{ app = \"com.apple.Safari\" }]\n",
        )
        .unwrap()
        .config;
        let new = effective(&cfg, None);
        let ws = [win(20, "T", "com.apple.Safari", "", "floating"), win(21, "T", "com.apple.TextEdit", "hand-tiled", "h_tiles")];
        let p = &plan_converge(&new, &mem, &ws)[0];
        assert_eq!(p.reason, Reason::Changed { fields: vec!["tiling-allowlist (top-level #1)".into()] });
        let cmds: Vec<&Cmd> = p.steps.iter().map(|(c, _)| c).collect();
        assert_eq!(cmds, [&Cmd::Tile { id: 20 }, &Cmd::Float { id: 21 }]);
    }

    #[test]
    fn converge_on_removal_retiles_only_its_own_floats() {
        let r = kitty_rule();
        let mut mem = TilingMemory::default();
        mem.records.insert("T".into(), RuleRecord::of(&r));
        mem.insert("T", 20, "com.apple.Safari");
        let ws = [win(20, "T", "com.apple.Safari", "", "floating"), win(21, "T", "com.apple.Preview", "", "floating")];
        let p = &plan_converge(&[], &mem, &ws)[0];
        assert_eq!(p.reason, Reason::Removed);
        assert_eq!(p.steps, [(Cmd::Tile { id: 20 }, "com.apple.Safari".to_string())]);
        commit(&mut mem, p);
        assert_eq!(mem, TilingMemory::default(), "record and memory dropped");
    }

    #[test]
    fn memory_follows_each_command() {
        let mut mem = TilingMemory::default();
        note(&mut mem, "T", &Cmd::Float { id: 1 }, "a");
        assert_eq!(mem.floated_on(1, "a"), Some("T"));
        note(&mut mem, "T", &Cmd::Tile { id: 1 }, "a");
        assert!(mem.rule_floated.is_empty());
    }

    #[test]
    fn view_gives_every_window_a_verdict_from_the_closed_set() {
        let r = kitty_rule();
        let mut mem = TilingMemory::default();
        mem.records.insert("T".into(), RuleRecord::of(&r));
        mem.insert("T", 4380, KITTY);
        mem.insert("T", 5012, "com.apple.Safari");
        mem.insert("T", 5300, KITTY);
        let wins = [
            win(4211, "T", KITTY, "noomz: ~", "h_tiles"),
            win(4380, "T", KITTY, "noomz: htop", "floating"),
            win(5012, "T", "com.apple.Safari", "GitHub", "floating"),
            win(5120, "T", "com.apple.Preview", "x.pdf", "floating"),
            win(5230, "T", "com.apple.Finder", "Downloads", "h_tiles"),
            win(5240, "T", KITTY, "vim", "h_tiles"),
            win(5300, "T", KITTY, "kitty settings", "floating"),
        ];
        let v = view(std::slice::from_ref(&r), &mem, Some(&wins));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].converge, "up to date");
        let verdicts: Vec<(u32, &str)> = v[0].windows.as_ref().unwrap().iter().map(|w| (w.id, w.verdict.as_str())).collect();
        assert_eq!(
            verdicts,
            [
                (4211, "entry 2 (1/1)"),
                (4380, "rule-floated: entry 2 full"),
                (5012, "rule-floated: not allowlisted"),
                (5120, "yours (rule ignores)"),
                (5230, "tiled by you, not allowlisted"),
                (5240, "over entry 2 limit"),
                (5300, "rule-floated: entry 1 carve-out"),
            ]
        );
        assert_eq!(v[0].entries[1].tiled, Some(2), "counts whoever tiled them");
    }

    #[test]
    fn view_converge_line_names_the_pending_reason() {
        let r = kitty_rule();
        let first = view(std::slice::from_ref(&r), &TilingMemory::default(), None);
        assert_eq!(first[0].converge, "pending: first run");
        let mut mem = TilingMemory::default();
        let old = Rule { tiling_allowlist: vec![], ..r.clone() };
        mem.records.insert("T".into(), RuleRecord::of(&old));
        assert_eq!(view(std::slice::from_ref(&r), &mem, None)[0].converge, "pending: rule changed: tiling-allowlist (top-level #1)");
        mem.insert("T", 1, "a");
        let removed = view(&[], &mem, Some(&[win(1, "T", "a", "", "floating")]));
        assert_eq!(removed[0].converge, "pending: rule removed, re-tiles 1");
        assert!(removed[0].rule.is_none());
    }
}
