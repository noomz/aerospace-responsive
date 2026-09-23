//! Cosmetic visibility restore (D26, ticket 13). Runs after the post-pass diff and any
//! re-pass; never verified, never warns, never triggers a pass. Guards: unchanged monitor
//! set, a real placement change, and "the pass itself disturbed this monitor".

use crate::aero::Cmd;
use crate::pass::Moved;
use crate::world::{self, AeroMonitor, WorkspaceRow};
use std::collections::{BTreeMap, BTreeSet};

/// What the user was looking at before the first pass of a disturbance (S2).
#[derive(Debug, Clone)]
pub struct RestoreCtx {
    pub monitors: Vec<AeroMonitor>,
    pub visible: BTreeMap<u32, String>,
    pub focused: Option<String>,
    /// Accumulated across a pass and its re-pass.
    pub moves: Vec<Moved>,
}

impl RestoreCtx {
    pub fn capture(monitors: Vec<AeroMonitor>, rows: &[WorkspaceRow]) -> Self {
        RestoreCtx {
            monitors,
            visible: rows.iter().filter(|r| r.visible).map(|r| (r.monitor, r.workspace.clone())).collect(),
            focused: world::focused(rows).map(|r| r.workspace.clone()),
            moves: vec![],
        }
    }
}

pub fn plan_restore(ctx: &RestoreCtx, monitors_now: &[AeroMonitor], rows: &[WorkspaceRow], nonempty: &BTreeSet<String>) -> (Vec<Cmd>, Vec<String>) {
    if monitors_now != ctx.monitors.as_slice() {
        return (vec![], vec!["monitor set changed since snapshot; restore skipped".into()]); // S3.1
    }
    if ctx.moves.is_empty() {
        return (vec![], vec!["the pass moved nothing to a different monitor; restore skipped".into()]); // S3.2
    }
    let touched: BTreeSet<u32> = ctx.moves.iter().flat_map(|m| [m.from, m.to]).collect();
    let assigned = |ws: &str| world::row(rows, ws).map(|r| r.monitor);
    let focused_monitor = ctx
        .focused
        .as_ref()
        .and_then(|f| ctx.visible.iter().find(|(_, w)| *w == f).map(|(m, _)| *m));

    let (mut cmds, mut skips) = (vec![], vec![]);
    for (&m, w) in &ctx.visible {
        if Some(m) == focused_monitor {
            continue; // restored last (S4)
        }
        let why = if !touched.contains(&m) {
            Some("not disturbed by the pass")
        } else if !nonempty.contains(w) {
            Some("previously-visible workspace is empty")
        } else if assigned(w) != Some(m) {
            Some("previously-visible workspace is no longer assigned here")
        } else if world::visible_on(rows, m) == Some(w.as_str()) {
            Some("already visible")
        } else {
            None
        };
        match why {
            Some(why) => skips.push(format!("monitor {m}: {why}")),
            None => cmds.push(Cmd::Focus { ws: w.clone() }),
        }
    }

    // S4: the last command is `workspace F`, or whatever F's monitor now shows, so focus
    // stays on the same monitor rather than wherever the last restore left it.
    if let (Some(f), Some(fm)) = (&ctx.focused, focused_monitor) {
        let now_focused = world::focused(rows).map(|r| r.workspace.as_str());
        let target = if assigned(f) == Some(fm) { Some(f.clone()) } else { world::visible_on(rows, fm).map(String::from) };
        match target {
            Some(t) if !(cmds.is_empty() && now_focused == Some(t.as_str())) => cmds.push(Cmd::Focus { ws: t }),
            Some(_) => skips.push(format!("monitor {fm}: focus already correct")),
            None => skips.push(format!("monitor {fm}: nothing to focus")),
        }
    }
    (cmds, skips)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aero::Aero;
    use crate::config::Axis;
    use crate::testutil::{FakeAero, desk3};

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn mv(ws: &str, from: u32, to: u32) -> Moved {
        Moved { ws: ws.into(), from, to }
    }

    /// Before: portrait shows A, built-in shows T (focused), landscape shows 2.
    fn before() -> FakeAero {
        FakeAero::new(
            &desk3(),
            &[
                ("A", 1, true, 1, Axis::H),
                ("1", 2, false, 1, Axis::H),
                ("3", 2, false, 1, Axis::H),
                ("T", 2, true, 1, Axis::H),
                ("2", 3, true, 1, Axis::H),
            ],
            "T",
        )
    }

    fn apply(f: &mut FakeAero, moves: &[Moved]) {
        for m in moves {
            f.run(&Cmd::Move { ws: m.ws.clone(), monitor: m.to, target: m.to.to_string() }).unwrap();
        }
    }

    #[test]
    fn restores_each_monitor_then_focus_last() {
        let mut f = before();
        let mut ctx = RestoreCtx::capture(f.monitors.clone(), &f.clone_rows());
        ctx.moves = vec![mv("1", 2, 1), mv("3", 2, 3)];
        let moves = ctx.moves.clone();
        apply(&mut f, &moves);
        let (cmds, _) = plan_restore(&ctx, &f.monitors.clone(), &f.clone_rows(), &set(&["A", "1", "3", "T", "2"]));
        assert_eq!(cmds, vec![Cmd::Focus { ws: "A".into() }, Cmd::Focus { ws: "2".into() }, Cmd::Focus { ws: "T".into() }]);
    }

    #[test]
    fn changed_monitor_set_skips_everything() {
        let f = before();
        let mut ctx = RestoreCtx::capture(f.monitors.clone(), &f.clone_rows());
        ctx.moves = vec![mv("1", 2, 1)];
        let (cmds, skips) = plan_restore(&ctx, &f.monitors[..2], &f.clone_rows(), &set(&["A"]));
        assert!(cmds.is_empty());
        assert!(skips[0].contains("monitor set changed"));
    }

    #[test]
    fn no_real_move_skips_everything() {
        let f = before();
        let ctx = RestoreCtx::capture(f.monitors.clone(), &f.clone_rows());
        assert!(plan_restore(&ctx, &f.monitors, &f.clone_rows(), &set(&["A"])).0.is_empty());
    }

    #[test]
    fn workspace_the_user_moved_mid_pass_is_left_alone() {
        let mut f = before();
        let mut ctx = RestoreCtx::capture(f.monitors.clone(), &f.clone_rows());
        ctx.moves = vec![mv("1", 2, 1)];
        let moves = ctx.moves.clone();
        apply(&mut f, &moves);
        apply(&mut f, &[mv("A", 1, 3)]); // the user, not the pass
        let (cmds, skips) = plan_restore(&ctx, &f.monitors.clone(), &f.clone_rows(), &set(&["A", "1", "3", "T", "2"]));
        assert!(!cmds.contains(&Cmd::Focus { ws: "A".into() }));
        assert!(skips.iter().any(|s| s.contains("no longer assigned here")));
    }

    #[test]
    fn focused_workspace_moved_away_focuses_what_its_monitor_shows() {
        let mut f = FakeAero::new(
            &desk3(),
            &[("1", 2, true, 1, Axis::H), ("T", 2, false, 1, Axis::H), ("A", 1, true, 1, Axis::H)],
            "1",
        );
        let mut ctx = RestoreCtx::capture(f.monitors.clone(), &f.clone_rows());
        ctx.moves = vec![mv("1", 2, 1)];
        let moves = ctx.moves.clone();
        apply(&mut f, &moves);
        let (cmds, _) = plan_restore(&ctx, &f.monitors.clone(), &f.clone_rows(), &set(&["1", "T", "A"]));
        assert_eq!(cmds.last(), Some(&Cmd::Focus { ws: "T".into() }));
    }
}
