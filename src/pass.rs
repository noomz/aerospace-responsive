//! The reconcile pass (D17, ticket 03): snapshot → placement → mandatory re-read →
//! exclusive-tiling converge → template rebuild → layout → post-pass diff (V44). Idempotence comes from desired-vs-observed diffing: a steady state issues
//! no commands, so repetition can never destroy manual sizing.

use crate::aero::{Aero, Cmd, monitor_target};
use crate::display::Display;
use crate::rules::Resolved;
use crate::state::{TemplateRecords, TilingMemory};
use crate::template;
use crate::tiling::{self, Plan};
use crate::world::{Snapshot, WinInfo};
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Step 2: non-empty opt-in workspaces that are not on their target. Empty ones are absent
/// from the snapshot, so they are skipped by construction (D10).
pub fn plan_placement(r: &Resolved, s: &Snapshot, ds: &[Display]) -> Vec<Cmd> {
    r.placement
        .iter()
        .filter_map(|p| {
            let target = p.target?;
            let current = s.monitor_of(&p.workspace)?;
            (current != target).then(|| Cmd::Move { ws: p.workspace.clone(), monitor: target, target: monitor_target(ds, target) })
        })
        .collect()
}

/// Step 4: unpinned non-empty workspaces whose root differs from the rule for the monitor
/// they are ACTUALLY on. An axis change gets `layout --root`, which keeps the layout kind (D18).
/// A rule's root kind (`kind`, else tiles under flatten) is enforced too: a window callback's
/// `layout accordion` or a flatten rule on another monitor can leave the wrong one. It is sent
/// only when wrong: a flatten leaves no child container, so it cannot change the root's kind.
/// A workspace under an applying template is the template's (V42).
pub fn plan_layout(r: &Resolved, s: &Snapshot, pinned: &BTreeSet<String>) -> Vec<Cmd> {
    let skip = r.hands_off(s, pinned);
    let mut cmds = vec![];
    for ws in s.nonempty() {
        if skip.contains(&ws) {
            continue;
        }
        let (Some(m), Some(axis)) = (s.monitor_of(&ws), s.root_axis(&ws)) else { continue };
        let Some(want) = r.want_for(m) else { continue };
        let axis_off = axis != want.axis;
        let kind = want.root_kind();
        let kind_off = kind.is_some_and(|k| s.root_kind(&ws) != Some(k));
        if !axis_off && !kind_off {
            continue;
        }
        if want.flatten {
            cmds.push(Cmd::Flatten { ws: ws.clone() });
        }
        if let Some(kind) = kind
            && kind_off
        {
            cmds.push(Cmd::LayoutKind { ws: ws.clone(), layout: kind });
        }
        if axis_off {
            cmds.push(Cmd::Layout { ws, axis: want.axis });
        }
    }
    cmds
}

/// Step 5: re-diff the FULL managed set — stub pulls and gcMonitors touch workspaces that
/// received no command. `dropped` = refused moves, unsatisfiable for this pass. Layout checks
/// skip pinned workspaces and those under an applying template (V42); placement checks do not.
pub fn diff(r: &Resolved, s: &Snapshot, pinned: &BTreeSet<String>, dropped: &BTreeSet<String>) -> Vec<String> {
    let mut out = vec![];
    for p in &r.placement {
        let (Some(t), Some(m)) = (p.target, s.monitor_of(&p.workspace)) else { continue };
        if m != t && !dropped.contains(&p.workspace) {
            out.push(format!("workspace {} is on monitor {m}, wanted {t}", p.workspace));
        }
    }
    let skip = r.hands_off(s, pinned);
    for ws in s.nonempty() {
        if skip.contains(&ws) {
            continue;
        }
        let (Some(m), Some(axis)) = (s.monitor_of(&ws), s.root_axis(&ws)) else { continue };
        let Some(want) = r.want_for(m) else { continue };
        if want.axis != axis {
            out.push(format!("workspace {ws} root axis is {axis:?}, wanted {:?}", want.axis));
        }
        if let Some(k) = want.root_kind()
            && let Some(got) = s.root_kind(&ws)
            && got != k
        {
            out.push(format!("workspace {ws} root is {}, wanted {}", got.as_str(), k.as_str()));
        }
    }
    out
}

/// V41 stale record: removes the record of a workspace with no effective template, or of a
/// non-empty, unpinned one whose template's `when` fails where it is (the `[[layout]]` rule owns
/// it there). Pinned and empty workspaces keep theirs. Returns whether any went.
fn drop_stale(r: &Resolved, s: &Snapshot, pinned: &BTreeSet<String>, applying: &BTreeSet<String>, records: &mut TemplateRecords) -> bool {
    let nonempty = s.nonempty();
    let before = records.len();
    records.retain(|ws, _| {
        let has_template = r.templates.iter().any(|d| &d.template.workspace == ws);
        has_template && (!nonempty.contains(ws) || pinned.contains(ws) || applying.contains(ws))
    });
    records.len() != before
}

/// Workspaces on a different monitor in `after` than in `before`, with nothing in between that
/// moves workspaces.
fn drifted(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    before
        .nonempty()
        .into_iter()
        .filter_map(|ws| {
            let (a, b) = (before.monitor_of(&ws)?, after.monitor_of(&ws)?);
            (a != b).then(|| format!("workspace {ws} moved from monitor {a} to {b}"))
        })
        .collect()
}

/// What a pass would issue if every move lands (`reconcile --dry-run`, observe mode).
/// `converge` = the pending exclusive-tiling plans, issued between placement and layout.
pub fn dry_run(r: &Resolved, s: &Snapshot, ds: &[Display], pinned: &BTreeSet<String>, converge: &[Plan]) -> Vec<Cmd> {
    let moves = plan_placement(r, s, ds);
    let mut landed = s.clone();
    for c in &moves {
        if let Cmd::Move { ws, monitor, .. } = c {
            for w in landed.windows.iter_mut().filter(|w| &w.workspace == ws) {
                w.monitor = *monitor;
            }
        }
    }
    let layout = plan_layout(r, &landed, pinned);
    let conv = converge.iter().flat_map(|p| p.steps.iter().map(|(c, _)| c.clone()));
    moves.into_iter().chain(conv).chain(layout).collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Moved {
    pub ws: String,
    pub from: u32,
    pub to: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Executed {
    pub cmd: String,
    pub error: Option<String>,
    /// How long AeroSpace took to answer. Each command re-lays out every monitor.
    pub ms: u64,
}

/// Runs `cmd`, timed.
fn timed(aero: &mut dyn Aero, cmd: &Cmd) -> (Result<(), String>, u64) {
    let t = Instant::now();
    let result = aero.run(cmd);
    (result, u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX))
}

#[derive(Debug, Default, Serialize)]
pub struct PassReport {
    pub executed: Vec<Executed>,
    /// Successful moves to a different monitor (restore guard S3.2).
    pub moves: Vec<Moved>,
    pub refused: Vec<(String, String)>,
    pub aborted: bool,
    /// `list-windows` came back empty (seen right at wake, ticket 12): retry, do not act.
    pub empty_snapshot: bool,
    pub mismatches: Vec<String>,
    /// Workspaces that changed monitor after placement: the layout step never moves one, so
    /// something else did (seen at dock: workspace 5 left the DELL mid-pass).
    pub drifted: Vec<String>,
    /// Workspaces whose exclusive-tiling converge completed this pass.
    pub converged: Vec<String>,
    /// Workspaces whose template rebuild completed this pass.
    pub rebuilt: Vec<String>,
    pub nonempty: BTreeSet<String>,
    /// What this pass left visible per monitor. Ticket 13 S3.3: a monitor is restored only
    /// while it still shows what the pass put there — anything else means the user acted.
    pub left_visible: BTreeMap<u32, String>,
    /// Wall time of the whole pass, reads included.
    pub ms: u64,
}

/// What a pass may change in persisted state, and how it saves template records mid-pass (V41).
pub struct Memory<'a> {
    pub tiling: &'a mut TilingMemory,
    pub templates: &'a mut TemplateRecords,
    pub save: &'a mut dyn FnMut(&TemplateRecords),
}

/// `mem` = persisted memory (`None` = no converge and no template rebuild at all); `live` = the
/// rule gets window events. While inert only removals converge (C3, V20).
pub fn run_pass(
    aero: &mut dyn Aero,
    r: &Resolved,
    ds: &[Display],
    pinned: &BTreeSet<String>,
    abort: &AtomicBool,
    mut mem: Option<Memory>,
    live: bool,
) -> Result<PassReport> {
    let started = Instant::now();
    let mut rep = PassReport::default();

    let s0 = aero.windows()?; // 1. SNAPSHOT
    if s0.windows.is_empty() {
        rep.empty_snapshot = true;
        return Ok(rep);
    }

    let mut dropped = BTreeSet::new(); // 2. PLACEMENT
    for cmd in plan_placement(r, &s0, ds) {
        if abort.load(Ordering::SeqCst) {
            rep.aborted = true;
            return Ok(rep);
        }
        let Cmd::Move { ws, monitor, .. } = &cmd else { unreachable!("placement plans only moves") };
        let (result, ms) = timed(aero, &cmd);
        match &result {
            Ok(()) => rep.moves.push(Moved { ws: ws.clone(), from: s0.monitor_of(ws).expect("planned from s0"), to: *monitor }),
            Err(e) => {
                dropped.insert(ws.clone());
                rep.refused.push((ws.clone(), e.clone()));
            }
        }
        rep.executed.push(Executed { cmd: cmd.to_string(), error: result.err(), ms });
    }

    let s1 = aero.windows()?; // 3. RE-READ (mandatory: placement changed which rule applies)

    // 3b. CONVERGE (V7, V10): before layout, so flatten and the root axis see the final tiled
    // set. Pins do not stop it (V11). A workspace's record moves only when its converge
    // completes, so an abort redoes it; a failed command is a pass error.
    if let Some(mem) = mem.as_mut().map(|m| &mut *m.tiling)
        && (!r.exclusive_tiling.is_empty() || !mem.records.is_empty())
    {
        let details = aero.window_details()?;
        for plan in tiling::plan_converge(&r.exclusive_tiling, mem, &details).into_iter().filter(|p| tiling::runnable(p, live)) {
            for (cmd, app) in &plan.steps {
                if abort.load(Ordering::SeqCst) {
                    rep.aborted = true;
                    return Ok(rep);
                }
                let (result, ms) = timed(aero, cmd);
                result.map_err(|e| anyhow!("{cmd}: {e}"))?;
                tiling::note(mem, &plan.workspace, cmd, app);
                rep.executed.push(Executed { cmd: cmd.to_string(), error: None, ms });
            }
            tiling::commit(mem, &plan);
            rep.converged.push(plan.workspace.clone());
        }
    }
    // Floating a root sibling can make AeroSpace change the root's kind or axis; layout decides
    // from what converge left, not from before it.
    let s_layout = if rep.converged.is_empty() { s1.clone() } else { aero.windows()? };

    // 3c. TEMPLATE REBUILD (V40 a, V41, V42): after converge, so the tiled set is final.
    if let Some(m) = mem.as_mut() {
        let applying = r.applying(&s_layout, pinned);
        if drop_stale(r, &s_layout, pinned, &applying, m.templates) {
            (m.save)(m.templates);
        }
        if !applying.is_empty() {
            let details = aero.window_details()?;
            for t in r.templates.iter().map(|d| &d.template).filter(|t| applying.contains(&t.workspace)) {
                let wins: Vec<WinInfo> = details.iter().filter(|w| w.workspace == t.workspace).cloned().collect();
                let want = template::wanted(t, &wins);
                if template::pending(m.templates.get(&t.workspace), &want).is_none() {
                    continue;
                }
                let cmds = template::rebuild(t, &wins);
                let done = template::execute(m.templates, &t.workspace, want, &cmds, m.save, |cmd| {
                    if abort.load(Ordering::SeqCst) {
                        return Ok(false);
                    }
                    let (result, ms) = timed(aero, cmd);
                    result.map_err(|e| anyhow!("{cmd}: {e}"))?;
                    rep.executed.push(Executed { cmd: cmd.to_string(), error: None, ms });
                    Ok(true)
                })?;
                if !done {
                    rep.aborted = true;
                    return Ok(rep);
                }
                if !cmds.is_empty() {
                    rep.rebuilt.push(t.workspace.clone());
                }
            }
        }
    }

    for cmd in plan_layout(r, &s_layout, pinned) {
        // 4. LAYOUT
        if abort.load(Ordering::SeqCst) {
            rep.aborted = true;
            return Ok(rep);
        }
        let (result, ms) = timed(aero, &cmd);
        rep.executed.push(Executed { cmd: cmd.to_string(), error: result.err(), ms });
    }

    let s2 = aero.windows()?; // 5. POST-PASS DIFF
    rep.mismatches = diff(r, &s2, pinned, &dropped);
    rep.drifted = drifted(&s1, &s2);
    rep.nonempty = s2.nonempty();
    if let Ok(rows) = aero.workspaces() {
        rep.left_visible = rows.iter().filter(|r| r.visible).map(|r| (r.monitor, r.workspace.clone())).collect();
    }
    rep.ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Axis, Kind, parse};
    use crate::rules::resolve;
    use crate::template::Tree;
    use crate::testutil::{FakeAero, desk3};

    fn cfg() -> crate::config::Config {
        parse(include_str!("../aerospace-responsive.example.toml")).unwrap().config
    }

    /// Just docked: 1 and 3 still on the built-in; C empty; A unmanaged on the portrait.
    fn docked() -> FakeAero {
        FakeAero::new(
            &desk3(),
            &[
                ("1", 2, false, 2, Axis::H),
                ("2", 3, true, 1, Axis::H),
                ("3", 2, false, 1, Axis::H),
                ("C", 2, false, 0, Axis::H),
                ("T", 2, true, 1, Axis::H),
                ("A", 1, true, 1, Axis::H),
            ],
            "T",
        )
    }

    fn pass(f: &mut FakeAero, pinned: &[&str]) -> PassReport {
        let ds = desk3();
        let r = resolve(&cfg(), &ds, None);
        let pinned = pinned.iter().map(|s| s.to_string()).collect();
        run_pass(f, &r, &ds, &pinned, &AtomicBool::new(false), None, false).unwrap()
    }

    fn mv(ws: &str, to: u32) -> Cmd {
        Cmd::Move { ws: ws.into(), monitor: to, target: to.to_string() }
    }

    #[test]
    fn first_pass_places_then_lays_out_from_the_landed_monitor() {
        let mut f = docked();
        let rep = pass(&mut f, &[]);
        assert_eq!(
            f.log,
            vec![
                mv("1", 1),
                mv("3", 3),
                Cmd::Flatten { ws: "1".into() },
                Cmd::Layout { ws: "1".into(), axis: Axis::V },
                Cmd::Flatten { ws: "A".into() },
                Cmd::Layout { ws: "A".into(), axis: Axis::V },
            ]
        );
        assert!(rep.mismatches.is_empty(), "{:?}", rep.mismatches);
        assert_eq!(rep.moves, vec![Moved { ws: "1".into(), from: 2, to: 1 }, Moved { ws: "3".into(), from: 2, to: 3 }]);
    }

    #[test]
    fn second_pass_is_a_no_op() {
        let mut f = docked();
        pass(&mut f, &[]);
        let n = f.log.len();
        let rep = pass(&mut f, &[]);
        assert_eq!(f.log.len(), n, "steady state must issue no commands");
        assert!(rep.executed.is_empty() && rep.mismatches.is_empty());
    }

    #[test]
    fn empty_managed_workspace_is_never_moved() {
        let mut f = docked();
        pass(&mut f, &[]);
        assert!(!f.log.iter().any(|c| matches!(c, Cmd::Move { ws, .. } if ws == "C")));
    }

    #[test]
    fn pinned_workspace_is_placed_but_not_reshaped() {
        let mut f = docked();
        pass(&mut f, &["1"]);
        assert!(f.log.contains(&mv("1", 1)));
        assert!(!f.log.iter().any(|c| matches!(c, Cmd::Flatten { ws } | Cmd::LayoutKind { ws, .. } | Cmd::Layout { ws, .. } if ws == "1")));
    }

    #[test]
    fn refused_move_is_dropped_and_not_reported_as_mismatch() {
        let mut f = docked();
        f.refuse.insert("3".into());
        let rep = pass(&mut f, &[]);
        assert_eq!(rep.refused.len(), 1);
        assert_eq!(rep.refused[0].0, "3");
        assert!(rep.mismatches.is_empty());
    }

    #[test]
    fn failed_move_takes_layout_from_where_the_workspace_actually_is() {
        let mut f = docked();
        f.refuse.insert("1".into());
        pass(&mut f, &[]);
        // 1 stayed on the built-in (h); it must NOT get the portrait's v layout.
        assert!(!f.log.iter().any(|c| matches!(c, Cmd::Layout { ws, .. } if ws == "1")));
    }

    #[test]
    fn abort_stops_before_the_next_command() {
        let mut f = docked();
        let ds = desk3();
        let r = resolve(&cfg(), &ds, None);
        let rep = run_pass(&mut f, &r, &ds, &BTreeSet::new(), &AtomicBool::new(true), None, false).unwrap();
        assert!(rep.aborted && f.log.is_empty());
    }

    #[test]
    fn portrait_accordion_on_the_right_axis_gets_flattened_once() {
        let mut f = docked();
        pass(&mut f, &[]);
        f.log.clear();
        let i = f.ws.iter().position(|w| w.name == "1").unwrap();
        f.ws[i].accordion = true;
        let rep = pass(&mut f, &[]);
        assert_eq!(f.log, vec![Cmd::Flatten { ws: "1".into() }, Cmd::LayoutKind { ws: "1".into(), layout: Kind::Tiles }]);
        assert!(rep.mismatches.is_empty(), "{:?}", rep.mismatches);
        f.log.clear();
        let again = pass(&mut f, &[]);
        assert!(again.executed.is_empty(), "flatten must not repeat: {:?}", again.executed);
    }

    #[test]
    fn stubborn_layout_shows_up_in_the_post_pass_diff() {
        let mut f = docked();
        f.ignore_layout = true;
        let rep = pass(&mut f, &[]);
        assert_eq!(rep.mismatches.len(), 2, "{:?}", rep.mismatches);
    }

    #[test]
    fn axis_change_without_flatten_keeps_the_accordion() {
        let mut f = docked();
        pass(&mut f, &[]);
        f.log.clear();
        let i = f.ws.iter().position(|w| w.name == "T").unwrap();
        f.ws[i].accordion = true;
        f.ws[i].root = Axis::V;
        let rep = pass(&mut f, &[]);
        assert_eq!(f.log, vec![Cmd::Layout { ws: "T".into(), axis: Axis::H }]);
        assert!(rep.mismatches.is_empty(), "{:?}", rep.mismatches);
        assert!(f.ws[i].accordion, "a rule without flatten never asks for tiles");
        f.log.clear();
        assert!(pass(&mut f, &[]).executed.is_empty(), "an accordion on the right axis is left alone");
    }

    /// The example config with `kind` on the built-in rule (the laptop-only setup's screen).
    fn pass_kind(f: &mut FakeAero, kind: &str) -> PassReport {
        let toml = include_str!("../aerospace-responsive.example.toml");
        let old = "orientation = \"h\"\nflatten     = false";
        assert!(toml.contains(old));
        let cfg = parse(&toml.replacen(old, &format!("{old}\nkind = \"{kind}\""), 1)).unwrap().config;
        let ds = desk3();
        let r = resolve(&cfg, &ds, None);
        run_pass(f, &r, &ds, &BTreeSet::new(), &AtomicBool::new(false), None, false).unwrap()
    }

    #[test]
    fn a_kind_rule_turns_a_tiled_arrival_into_an_accordion_once() {
        // Undock: T left the portrait flatten rule as v_tiles; the built-in rule wants h accordion.
        let mut f = docked();
        let i = f.ws.iter().position(|w| w.name == "T").unwrap();
        f.ws[i].root = Axis::V;
        let rep = pass_kind(&mut f, "accordion");
        let on_t: Vec<_> = f.log.iter().filter(|c| matches!(c, Cmd::LayoutKind { ws, .. } | Cmd::Layout { ws, .. } | Cmd::Flatten { ws } if ws == "T")).cloned().collect();
        assert_eq!(on_t, vec![Cmd::LayoutKind { ws: "T".into(), layout: Kind::Accordion }, Cmd::Layout { ws: "T".into(), axis: Axis::H }]);
        assert!(f.ws[i].accordion && f.ws[i].root == Axis::H);
        assert!(rep.mismatches.is_empty(), "{:?}", rep.mismatches);
        f.log.clear();
        assert!(pass_kind(&mut f, "accordion").executed.is_empty(), "accordion on the right axis is settled");
    }

    #[test]
    fn a_tiles_kind_undoes_a_callback_accordion_without_flattening() {
        let mut f = docked();
        pass_kind(&mut f, "tiles");
        f.log.clear();
        let i = f.ws.iter().position(|w| w.name == "T").unwrap();
        f.ws[i].accordion = true;
        pass_kind(&mut f, "tiles");
        assert_eq!(f.log, vec![Cmd::LayoutKind { ws: "T".into(), layout: Kind::Tiles }]);
    }

    #[test]
    fn stubborn_kind_shows_up_in_the_post_pass_diff() {
        let mut f = docked();
        f.ignore_layout = true;
        let rep = pass_kind(&mut f, "accordion");
        assert!(rep.mismatches.contains(&"workspace T root is tiles, wanted accordion".to_string()), "{:?}", rep.mismatches);
    }

    #[test]
    fn stubborn_accordion_shows_up_in_the_post_pass_diff() {
        let mut f = docked();
        pass(&mut f, &[]);
        let i = f.ws.iter().position(|w| w.name == "1").unwrap();
        f.ws[i].accordion = true;
        f.ignore_layout = true;
        let rep = pass(&mut f, &[]);
        assert_eq!(rep.mismatches, ["workspace 1 root is accordion, wanted tiles"]);
    }

    #[test]
    fn a_workspace_that_moves_during_the_layout_step_is_reported_as_drift() {
        let mut f = docked();
        f.drift_on_layout = Some(("3".into(), 2)); // placed on 3, then AeroSpace puts it back on 2
        let rep = pass(&mut f, &[]);
        assert_eq!(rep.drifted, ["workspace 3 moved from monitor 3 to 2"]);
        assert!(rep.executed.iter().all(|e| e.ms < 5_000), "every command carries its time");
        f.log.clear();
        assert!(pass(&mut f, &[]).drifted.is_empty(), "a quiet pass reports no drift");
    }

    #[test]
    fn empty_snapshot_does_nothing() {
        let mut f = FakeAero::new(&desk3(), &[("1", 2, true, 0, Axis::H)], "1");
        let rep = pass(&mut f, &[]);
        assert!(rep.empty_snapshot && f.log.is_empty());
    }

    #[test]
    fn dry_run_predicts_the_real_pass() {
        let mut f = docked();
        let ds = desk3();
        let r = resolve(&cfg(), &ds, None);
        let predicted = dry_run(&r, &f.windows().unwrap(), &ds, &BTreeSet::new(), &[]);
        pass(&mut f, &[]);
        assert_eq!(predicted, f.log);
    }

    // ---------- exclusive tiling converge ----------

    const KITTY: &str = "net.kovidgoyal.kitty";
    const RULE: &str = "\n[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [{ app = \"net.kovidgoyal.kitty\", max-tiled = 1 }]\n";

    fn cfg_with(extra: &str) -> crate::config::Config {
        parse(&format!("{}{extra}", include_str!("../aerospace-responsive.example.toml"))).unwrap().config
    }

    /// docked() plus two kitty windows on T; T's first window (105) is some other app.
    fn docked_kitty() -> FakeAero {
        let mut f = docked();
        f.add_window("T", 300, KITTY, "noomz: ~", false);
        f.add_window("T", 200, KITTY, "noomz: htop", false);
        f
    }

    /// Tiling memory alone: no template records kept, saves dropped.
    fn tmem(mem: &mut TilingMemory) -> Memory<'_> {
        Memory { tiling: mem, templates: Box::leak(Box::default()), save: Box::leak(Box::new(|_: &TemplateRecords| {})) }
    }

    fn tiling_pass(f: &mut FakeAero, cfg: &crate::config::Config, mem: &mut TilingMemory, pinned: &[&str], abort: bool) -> PassReport {
        let ds = desk3();
        let r = resolve(cfg, &ds, None);
        let pinned = pinned.iter().map(|s| s.to_string()).collect();
        run_pass(f, &r, &ds, &pinned, &AtomicBool::new(abort), Some(tmem(mem)), true).unwrap()
    }

    #[test]
    fn layout_sees_the_kind_converge_left() {
        // T's rule wants tiles; floating a window there turns its root accordion mid-pass.
        let old = "orientation = \"h\"\nflatten     = false";
        let cfg = cfg_with(RULE);
        let toml = format!("{}{RULE}", include_str!("../aerospace-responsive.example.toml")).replacen(old, &format!("{old}\nkind = \"tiles\""), 1);
        let cfg_tiles = parse(&toml).unwrap().config;
        assert_eq!(cfg.layout.len(), cfg_tiles.layout.len());
        let mut f = docked_kitty();
        f.accordion_on_float = Some("T".into());
        let mut mem = TilingMemory::default();
        let rep = tiling_pass(&mut f, &cfg_tiles, &mut mem, &[], false);
        assert!(f.log.contains(&Cmd::LayoutKind { ws: "T".into(), layout: Kind::Tiles }), "{:?}", f.log);
        assert!(rep.mismatches.is_empty(), "{:?}", rep.mismatches);
    }

    #[test]
    fn first_run_converges_between_placement_and_layout() {
        let mut f = docked_kitty();
        let mut mem = TilingMemory::default();
        let rep = tiling_pass(&mut f, &cfg_with(RULE), &mut mem, &[], false);
        assert_eq!(
            f.log,
            vec![
                mv("1", 1),
                mv("3", 3),
                Cmd::Float { id: 105 }, // not allowlisted
                Cmd::Float { id: 300 }, // 200 is the lower id: it keeps the one kitty slot
                Cmd::Flatten { ws: "1".into() },
                Cmd::Layout { ws: "1".into(), axis: Axis::V },
                Cmd::Flatten { ws: "A".into() },
                Cmd::Layout { ws: "A".into(), axis: Axis::V },
            ]
        );
        assert_eq!(rep.converged, ["T"]);
        assert!(rep.mismatches.is_empty(), "the diff never checks the rule: {:?}", rep.mismatches);
        assert!(mem.records.contains_key("T"));
        let n = f.log.len();
        tiling_pass(&mut f, &cfg_with(RULE), &mut mem, &[], false);
        assert_eq!(f.log.len(), n, "equal record: no converge on the next pass (a restart is the same)");
    }

    #[test]
    fn a_pinned_workspace_still_converges() {
        let mut f = docked_kitty();
        let mut mem = TilingMemory::default();
        tiling_pass(&mut f, &cfg_with(RULE), &mut mem, &["T"], false);
        assert!(f.log.contains(&Cmd::Float { id: 105 }));
    }

    #[test]
    fn inert_rule_converges_nothing_but_a_removal_still_retiles() {
        let mut f = docked_kitty();
        let ds = desk3();
        let mut mem = TilingMemory::default();
        let r = resolve(&cfg_with(RULE), &ds, None);
        run_pass(&mut f, &r, &ds, &BTreeSet::new(), &AtomicBool::new(false), Some(tmem(&mut mem)), false).unwrap();
        assert!(!f.log.iter().any(|c| matches!(c, Cmd::Float { .. } | Cmd::Tile { .. })), "inert: no first run");
        tiling_pass(&mut f, &cfg_with(RULE), &mut mem, &[], false); // live: first run floats 105 and 300
        let n = f.log.len();
        let removed = resolve(&cfg(), &ds, None);
        run_pass(&mut f, &removed, &ds, &BTreeSet::new(), &AtomicBool::new(false), Some(tmem(&mut mem)), false).unwrap();
        assert_eq!(f.log[n..], [Cmd::Tile { id: 105 }, Cmd::Tile { id: 300 }], "V20: removal needs no events");
        assert_eq!(mem, TilingMemory::default());
    }

    #[test]
    fn edit_and_profile_switch_converge_removal_retiles() {
        let mut f = docked_kitty();
        let mut mem = TilingMemory::default();
        tiling_pass(&mut f, &cfg_with(RULE), &mut mem, &[], false);
        // A profile matching desk3 raises kitty's limit: the effective rule changed.
        let prof = format!("{RULE}\n[[profile]]\nname = \"desk3\"\nwhen = {{ count = 3 }}\n[[profile.exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [{{ app = \"net.kovidgoyal.kitty\" }}]\n");
        let n = f.log.len();
        let rep = tiling_pass(&mut f, &cfg_with(&prof), &mut mem, &[], false);
        assert_eq!(f.log[n..], [Cmd::Tile { id: 300 }], "rule-floated kitty now admitted");
        assert_eq!(rep.converged, ["T"]);
        // Rule removed: the rule's own floats re-tile, its record goes.
        let n = f.log.len();
        tiling_pass(&mut f, &cfg(), &mut mem, &[], false);
        assert_eq!(f.log[n..], [Cmd::Tile { id: 105 }]);
        assert_eq!(mem, TilingMemory::default());
    }

    #[test]
    fn an_aborted_converge_is_redone() {
        let mut f = docked_kitty();
        pass(&mut f, &[]); // placement and layout settle first
        let n = f.log.len();
        let mut mem = TilingMemory::default();
        let rep = tiling_pass(&mut f, &cfg_with(RULE), &mut mem, &[], true);
        assert!(rep.aborted && f.log.len() == n);
        assert!(mem.records.is_empty(), "record moves only when the converge completes");
        tiling_pass(&mut f, &cfg_with(RULE), &mut mem, &[], false);
        assert_eq!(f.log[n..], [Cmd::Float { id: 105 }, Cmd::Float { id: 300 }]);
    }

    #[test]
    fn a_failed_converge_command_is_a_pass_error() {
        let mut mem = TilingMemory::default();
        let ds = desk3();
        let r = resolve(&cfg_with(RULE), &ds, None);
        let mut fake_err = FailTile(docked_kitty()); // e.g. the window closed after the read
        let err = run_pass(&mut fake_err, &r, &ds, &BTreeSet::new(), &AtomicBool::new(false), Some(tmem(&mut mem)), true).unwrap_err();
        assert!(format!("{err:#}").contains("layout floating --window-id 105"), "{err:#}");
        assert!(mem.records.is_empty());
    }

    // ---------- workspace template rebuild (T36) ----------

    /// Built-in wants `h`, the portrait flattens to `v`; `C` on the built-in gets `v[LINE, h[IGCL, Msgr]]`,
    /// so a layout check that looked at `C` would report its root.
    const TPL: &str = r#"
[[layout]]
when = { portrait = true }
orientation = "v"
flatten = true
[[layout]]
when = { builtin = true }
orientation = "h"
flatten = false
[[workspace-template]]
workspaces = ["C"]
when = { builtin = true }
root = "v"
tree = [{ app = "line" }, [{ app = "igcl" }, { app = "msgr" }]]
"#;

    fn tpl_cfg(extra: &str) -> crate::config::Config {
        parse(&format!("{TPL}{extra}")).unwrap().config
    }

    /// `C` on the built-in (2) with LINE 44, IGCL 10, Messenger 49, flat under an `h` root.
    fn tpl_fake(monitor: u32) -> FakeAero {
        let mut f = FakeAero::new(&desk3(), &[("C", monitor, true, 0, Axis::H)], "C");
        f.add_window("C", 44, "line", "LINE", false);
        f.add_window("C", 10, "igcl", "IGCL", false);
        f.add_window("C", 49, "msgr", "Messenger", false);
        f
    }

    fn built() -> Tree {
        Tree::Split(Axis::V, vec![Tree::Win(44), Tree::Split(Axis::H, vec![Tree::Win(10), Tree::Win(49)])])
    }

    /// Persisted template records plus every save the pass made, newest last.
    #[derive(Default)]
    struct Disk {
        records: TemplateRecords,
        saves: Vec<TemplateRecords>,
    }

    fn tpass(f: &mut dyn Aero, cfg: &crate::config::Config, disk: &mut Disk, pinned: &[&str], abort: &AtomicBool) -> Result<PassReport> {
        let ds = desk3();
        let r = resolve(cfg, &ds, None);
        let pinned = pinned.iter().map(|s| s.to_string()).collect();
        let mut tiling = TilingMemory::default();
        let saves = &mut disk.saves;
        let mut save = |recs: &TemplateRecords| saves.push(recs.clone());
        let mem = Memory { tiling: &mut tiling, templates: &mut disk.records, save: &mut save };
        run_pass(f, &r, &ds, &pinned, abort, Some(mem), true)
    }

    fn ok_pass(f: &mut FakeAero, cfg: &crate::config::Config, disk: &mut Disk, pinned: &[&str]) -> PassReport {
        tpass(f, cfg, disk, pinned, &AtomicBool::new(false)).unwrap()
    }

    fn template_cmds(log: &[Cmd]) -> usize {
        log.iter().filter(|c| matches!(c, Cmd::MoveWindow { .. } | Cmd::JoinWith { .. })).count()
    }

    #[test]
    fn first_run_rebuilds_and_the_layout_rule_leaves_the_workspace_alone() {
        let mut f = tpl_fake(2);
        let mut disk = Disk::default();
        let rep = ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
        assert_eq!(f.tree("C"), built());
        assert_eq!(rep.rebuilt, ["C"]);
        assert!(rep.mismatches.is_empty(), "no mismatch for an applied template: {:?}", rep.mismatches);
        assert!(!f.log.contains(&Cmd::Layout { ws: "C".into(), axis: Axis::H }), "the h rule must not touch C: {:?}", f.log);
        assert_eq!(disk.saves.last(), Some(&disk.records), "saved after the last command");
        assert!(disk.records.contains_key("C"));
        // Verify-only uses the same diff: the v root on an h monitor is not a mismatch there either.
        let r = resolve(&tpl_cfg(""), &desk3(), None);
        assert!(diff(&r, &f.windows().unwrap(), &BTreeSet::new(), &BTreeSet::new()).is_empty());
        f.log.clear();
        let again = ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
        assert!(f.log.is_empty() && again.rebuilt.is_empty(), "equal record: nothing to do: {:?}", f.log);
    }

    #[test]
    fn a_daemon_restart_sends_no_commands() {
        let mut f = tpl_fake(2);
        let mut disk = Disk::default();
        ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
        let mut restarted = Disk { records: disk.saves.last().unwrap().clone(), saves: vec![] };
        f.log.clear();
        ok_pass(&mut f, &tpl_cfg(""), &mut restarted, &[]);
        assert!(f.log.is_empty() && restarted.saves.is_empty(), "{:?}", f.log);
    }

    #[test]
    fn a_template_edit_rebuilds() {
        let mut f = tpl_fake(2);
        let mut disk = Disk::default();
        ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
        let edited = parse(&TPL.replace(r#"tree = [{ app = "line" }, [{ app = "igcl" }, { app = "msgr" }]]"#, r#"tree = [[{ app = "igcl" }, { app = "msgr" }], { app = "line" }]"#)).unwrap().config;
        f.log.clear();
        let rep = ok_pass(&mut f, &edited, &mut disk, &[]);
        assert_eq!(rep.rebuilt, ["C"]);
        assert_eq!(f.tree("C"), Tree::Split(Axis::V, vec![Tree::Split(Axis::H, vec![Tree::Win(10), Tree::Win(49)]), Tree::Win(44)]));
    }

    #[test]
    fn dock_undock_dock_rebuilds_on_every_return_to_the_builtin() {
        let mut f = tpl_fake(2);
        let mut disk = Disk::default();
        ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
        let i = f.ws.iter().position(|w| w.name == "C").unwrap();
        for _ in 0..2 {
            f.ws[i].monitor = 1; // dock: C on the portrait, whose layout rule owns it
            f.log.clear();
            let rep = ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
            assert_eq!(template_cmds(&f.log), 0, "{:?}", f.log);
            assert!(rep.rebuilt.is_empty() && rep.mismatches.is_empty(), "{:?}", rep.mismatches);
            assert!(disk.records.is_empty() && disk.saves.last() == Some(&TemplateRecords::new()), "stale record removed and saved");
            f.run(&Cmd::Flatten { ws: "C".into() }).unwrap(); // the user, or the rule on an h template
            f.ws[i].monitor = 2; // undock
            let rep = ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
            assert_eq!(rep.rebuilt, ["C"]);
            assert_eq!(f.tree("C"), built());
        }
    }

    #[test]
    fn when_is_judged_on_the_monitor_placement_lands_on() {
        let mut f = tpl_fake(1);
        let mut disk = Disk::default();
        let rep = ok_pass(&mut f, &tpl_cfg("[[placement]]\nworkspaces = [\"C\"]\nmonitor = { builtin = true }\n"), &mut disk, &[]);
        assert!(matches!(&f.log[0], Cmd::Move { ws, monitor: 2, .. } if ws == "C"), "{:?}", f.log);
        assert_eq!(rep.rebuilt, ["C"]);
        assert_eq!(f.tree("C"), built());
    }

    #[test]
    fn a_pinned_workspace_is_neither_rebuilt_nor_laid_out() {
        let mut f = tpl_fake(2);
        let mut disk = Disk::default();
        let rep = ok_pass(&mut f, &tpl_cfg(""), &mut disk, &["C"]);
        assert!(f.log.is_empty() && rep.rebuilt.is_empty(), "{:?}", f.log);
        assert!(disk.records.is_empty());
        // A pin made after a rebuild keeps the record (V46: unpin removes it).
        ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
        f.log.clear();
        let i = f.ws.iter().position(|w| w.name == "C").unwrap();
        f.ws[i].tree = None; // hand-made change
        ok_pass(&mut f, &tpl_cfg(""), &mut disk, &["C"]);
        assert!(f.log.is_empty() && disk.records.contains_key("C"));
    }

    #[test]
    fn removing_the_template_drops_the_record_without_commands() {
        let mut f = tpl_fake(2);
        let mut disk = Disk::default();
        ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
        f.log.clear();
        let gone = parse(&TPL[..TPL.find("[[workspace-template]]").unwrap()]).unwrap().config;
        ok_pass(&mut f, &gone, &mut disk, &[]);
        assert_eq!(template_cmds(&f.log), 0, "V50: {:?}", f.log);
        assert!(disk.records.is_empty());
    }

    #[test]
    fn a_command_failing_halfway_leaves_no_record_and_the_repass_rebuilds() {
        let mut f = Trip { f: tpl_fake(2), on: |c| matches!(c, Cmd::JoinWith { .. }), trap: Trap::Fail, abort: None };
        let mut disk = Disk::default();
        let err = tpass(&mut f, &tpl_cfg(""), &mut disk, &[], &AtomicBool::new(false)).unwrap_err();
        assert!(format!("{err:#}").contains("join-with"), "{err:#}");
        assert!(disk.records.is_empty() && disk.saves.iter().all(|s| s.is_empty()));
        let mut f = f.f;
        f.log.clear();
        let rep = ok_pass(&mut f, &tpl_cfg(""), &mut disk, &[]);
        assert_eq!(rep.rebuilt, ["C"]);
        assert_eq!(f.log[0], Cmd::Flatten { ws: "C".into() }, "a full rebuild from the top");
        assert_eq!(f.tree("C"), built());
    }

    #[test]
    fn an_abort_mid_rebuild_stops_and_leaves_no_record() {
        let abort = AtomicBool::new(false);
        let mut f = Trip { f: tpl_fake(2), on: |c| matches!(c, Cmd::MoveWindow { .. }), trap: Trap::Abort, abort: Some(&abort) };
        let mut disk = Disk::default();
        ok_pass(&mut f.f, &tpl_cfg(""), &mut disk, &[]);
        disk.records.clear(); // force a rebuild
        f.f.log.clear();
        let rep = tpass(&mut f, &tpl_cfg(""), &mut disk, &[], &abort).unwrap();
        assert!(rep.aborted && rep.rebuilt.is_empty());
        assert_eq!(template_cmds(&f.f.log), 1, "nothing after the abort: {:?}", f.f.log);
        assert!(disk.records.is_empty());
    }

    enum Trap {
        Fail,
        Abort,
    }

    /// FakeAero that fails, or raises the abort flag, on the commands `on` picks.
    struct Trip<'a> {
        f: FakeAero,
        on: fn(&Cmd) -> bool,
        trap: Trap,
        abort: Option<&'a AtomicBool>,
    }
    impl Aero for Trip<'_> {
        fn monitors(&mut self) -> Result<Vec<crate::world::AeroMonitor>> {
            self.f.monitors()
        }
        fn windows(&mut self) -> Result<Snapshot> {
            self.f.windows()
        }
        fn workspaces(&mut self) -> Result<Vec<crate::world::WorkspaceRow>> {
            self.f.workspaces()
        }
        fn window_details(&mut self) -> Result<Vec<crate::world::WinInfo>> {
            self.f.window_details()
        }
        fn run(&mut self, cmd: &Cmd) -> Result<(), String> {
            if (self.on)(cmd) {
                match self.trap {
                    Trap::Fail => return Err("Invalid window-id".into()),
                    Trap::Abort => self.abort.unwrap().store(true, Ordering::SeqCst),
                }
            }
            self.f.run(cmd)
        }
        fn subscribe(&mut self, sink: crate::aero::Sink) -> Result<crate::aero::Subscription> {
            self.f.subscribe(sink)
        }
    }

    /// FakeAero whose window-id commands fail.
    struct FailTile(FakeAero);
    impl Aero for FailTile {
        fn monitors(&mut self) -> Result<Vec<crate::world::AeroMonitor>> {
            self.0.monitors()
        }
        fn windows(&mut self) -> Result<Snapshot> {
            self.0.windows()
        }
        fn workspaces(&mut self) -> Result<Vec<crate::world::WorkspaceRow>> {
            self.0.workspaces()
        }
        fn window_details(&mut self) -> Result<Vec<crate::world::WinInfo>> {
            self.0.window_details()
        }
        fn run(&mut self, cmd: &Cmd) -> Result<(), String> {
            match cmd {
                Cmd::Float { .. } | Cmd::Tile { .. } => Err("Invalid window-id".into()),
                c => self.0.run(c),
            }
        }
        fn subscribe(&mut self, sink: crate::aero::Sink) -> Result<crate::aero::Subscription> {
            self.0.subscribe(sink)
        }
    }
}
