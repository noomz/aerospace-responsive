//! The reconcile pass (D17, ticket 03): snapshot → placement → mandatory re-read → layout →
//! post-pass diff. Idempotence comes from desired-vs-observed diffing: a steady state issues
//! no commands, so repetition can never destroy manual sizing.

use crate::aero::{Aero, Cmd, monitor_target};
use crate::display::Display;
use crate::rules::Resolved;
use crate::world::Snapshot;
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Step 4: unpinned non-empty workspaces whose root axis differs from the rule for the
/// monitor they are ACTUALLY on. Flatten only on rule policy AND an axis change (D18).
pub fn plan_layout(r: &Resolved, s: &Snapshot, pinned: &BTreeSet<String>) -> Vec<Cmd> {
    let mut cmds = vec![];
    for ws in s.nonempty() {
        if pinned.contains(&ws) {
            continue;
        }
        let (Some(m), Some(axis)) = (s.monitor_of(&ws), s.root_axis(&ws)) else { continue };
        let Some(want) = r.want_for(m) else { continue };
        if axis == want.axis {
            continue;
        }
        if want.flatten {
            cmds.push(Cmd::Flatten { ws: ws.clone() });
        }
        cmds.push(Cmd::Layout { ws, axis: want.axis });
    }
    cmds
}

/// Step 5: re-diff the FULL managed set — stub pulls and gcMonitors touch workspaces that
/// received no command. `dropped` = refused moves, unsatisfiable for this pass.
pub fn diff(r: &Resolved, s: &Snapshot, pinned: &BTreeSet<String>, dropped: &BTreeSet<String>) -> Vec<String> {
    let mut out = vec![];
    for p in &r.placement {
        let (Some(t), Some(m)) = (p.target, s.monitor_of(&p.workspace)) else { continue };
        if m != t && !dropped.contains(&p.workspace) {
            out.push(format!("workspace {} is on monitor {m}, wanted {t}", p.workspace));
        }
    }
    for ws in s.nonempty() {
        if pinned.contains(&ws) {
            continue;
        }
        let (Some(m), Some(axis)) = (s.monitor_of(&ws), s.root_axis(&ws)) else { continue };
        if let Some(want) = r.want_for(m)
            && want.axis != axis
        {
            out.push(format!("workspace {ws} root axis is {axis:?}, wanted {:?}", want.axis));
        }
    }
    out
}

/// What a pass would issue if every move lands (`reconcile --dry-run`, observe mode).
pub fn dry_run(r: &Resolved, s: &Snapshot, ds: &[Display], pinned: &BTreeSet<String>) -> Vec<Cmd> {
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
    moves.into_iter().chain(layout).collect()
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
    pub nonempty: BTreeSet<String>,
    /// What this pass left visible per monitor. Ticket 13 S3.3: a monitor is restored only
    /// while it still shows what the pass put there — anything else means the user acted.
    pub left_visible: BTreeMap<u32, String>,
}

pub fn run_pass(aero: &mut dyn Aero, r: &Resolved, ds: &[Display], pinned: &BTreeSet<String>, abort: &AtomicBool) -> Result<PassReport> {
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
        let result = aero.run(&cmd);
        match &result {
            Ok(()) => rep.moves.push(Moved { ws: ws.clone(), from: s0.monitor_of(ws).expect("planned from s0"), to: *monitor }),
            Err(e) => {
                dropped.insert(ws.clone());
                rep.refused.push((ws.clone(), e.clone()));
            }
        }
        rep.executed.push(Executed { cmd: cmd.to_string(), error: result.err() });
    }

    let s1 = aero.windows()?; // 3. RE-READ (mandatory: placement changed which rule applies)
    for cmd in plan_layout(r, &s1, pinned) {
        // 4. LAYOUT
        if abort.load(Ordering::SeqCst) {
            rep.aborted = true;
            return Ok(rep);
        }
        let result = aero.run(&cmd);
        rep.executed.push(Executed { cmd: cmd.to_string(), error: result.err() });
    }

    let s2 = aero.windows()?; // 5. POST-PASS DIFF
    rep.mismatches = diff(r, &s2, pinned, &dropped);
    rep.nonempty = s2.nonempty();
    if let Ok(rows) = aero.workspaces() {
        rep.left_visible = rows.iter().filter(|r| r.visible).map(|r| (r.monitor, r.workspace.clone())).collect();
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Axis, parse};
    use crate::rules::resolve;
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
        run_pass(f, &r, &ds, &pinned, &AtomicBool::new(false)).unwrap()
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
        assert!(!f.log.iter().any(|c| matches!(c, Cmd::Flatten { ws } | Cmd::Layout { ws, .. } if ws == "1")));
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
        let rep = run_pass(&mut f, &r, &ds, &BTreeSet::new(), &AtomicBool::new(true)).unwrap();
        assert!(rep.aborted && f.log.is_empty());
    }

    #[test]
    fn stubborn_layout_shows_up_in_the_post_pass_diff() {
        let mut f = docked();
        f.ignore_layout = true;
        let rep = pass(&mut f, &[]);
        assert_eq!(rep.mismatches.len(), 2, "{:?}", rep.mismatches);
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
        let predicted = dry_run(&r, &f.windows().unwrap(), &ds, &BTreeSet::new());
        pass(&mut f, &[]);
        assert_eq!(predicted, f.log);
    }
}
