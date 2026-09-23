//! Human text for every read verb. The daemon renders; clients only print (D25 V2).

use crate::aero::Cmd;
use crate::config::Axis;
use crate::display::Display;
use crate::rules::{ProfileMode, Resolved};
use crate::state::Warning;
use serde::Serialize;
use std::fmt::Write;

#[derive(Debug, Clone, Serialize)]
pub struct LastPass {
    pub at: String,
    pub trigger: String,
    pub result: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PinView {
    pub workspace: String,
    pub monitor: String,
    /// The workspace is currently off its pinned monitor (layout stays suppressed; P-7).
    pub suspended: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusView {
    pub profile: Option<String>,
    pub profile_mode: ProfileMode,
    pub lock: Option<String>,
    pub last_pass: Option<LastPass>,
    pub pins: Vec<PinView>,
    pub warnings: Vec<Warning>,
    pub aerospace_running: bool,
    pub observe: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorView {
    pub display: Display,
    pub portrait: bool,
    pub external: bool,
    pub pixel_area: u64,
    pub labels: Vec<String>,
}

fn axis(a: Axis) -> &'static str {
    if a == Axis::H { "h" } else { "v" }
}

fn profile_line(name: &Option<String>, mode: &ProfileMode) -> String {
    match (name, mode) {
        (Some(n), ProfileMode::Locked) => format!("{n} (locked)"),
        (Some(n), _) => format!("{n} (auto)"),
        (None, _) => "none (no profile matches)".into(),
    }
}

pub fn monitors(views: &[MonitorView]) -> String {
    let mut o = String::new();
    for m in views {
        let (d, s) = (&m.display, &m.display.screen);
        let _ = writeln!(o, "monitor {}  {}  (cg {})", d.aero_id, d.aero_name, s.cg_id);
        let _ = writeln!(
            o,
            "    {}x{} px  {}x{} pt at ({}, {})  rotation {}",
            s.px_w, s.px_h, s.bounds.w, s.bounds.h, s.bounds.x, s.bounds.y, s.rotation
        );
        let roles: Vec<&str> = [(s.builtin, "builtin"), (m.external, "external"), (m.portrait, "portrait"), (!m.portrait, "landscape"), (s.main, "main")]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, n)| *n)
            .collect();
        let _ = writeln!(o, "    {}  vendor {} model {} serial {}", roles.join(" "), s.vendor, s.model, s.serial);
        if !m.labels.is_empty() {
            let labels: Vec<String> = m.labels.iter().map(|l| format!("@{l}")).collect();
            let _ = writeln!(o, "    labels: {}", labels.join(" "));
        }
    }
    o
}

pub fn explain(r: &Resolved) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "profile: {}", profile_line(&r.profile, &r.profile_mode));
    for t in &r.profiles {
        let verdict = t.why_not.as_deref().map_or("matched".to_string(), |w| format!("no: {w}"));
        let _ = writeln!(o, "    {:<14} {verdict}", t.name);
    }
    let _ = writeln!(o, "layout:");
    for l in &r.layout {
        let flat = if l.want.flatten { ", flatten" } else { "" };
        let _ = writeln!(o, "  monitor {} {}  ->  {}{flat}   [{}]", l.monitor, l.monitor_name, axis(l.want.axis), l.source);
        for c in &l.considered {
            let _ = writeln!(o, "      {:<28} {}", c.rule, c.outcome);
        }
    }
    let _ = writeln!(o, "placement:");
    for p in &r.placement {
        match p.target {
            Some(t) => {
                let _ = writeln!(o, "  {:<4} ->  monitor {t}   [{}]", p.workspace, p.source.as_deref().unwrap_or(""));
            }
            None => {
                let _ = writeln!(o, "  {:<4} ->  (nothing satisfiable: stays where it is)", p.workspace);
            }
        }
        for c in &p.considered {
            let _ = writeln!(o, "      {:<28} {}", c.rule, c.outcome);
        }
    }
    for n in &r.no_match {
        let _ = writeln!(o, "warning: {n}");
    }
    for i in &r.info {
        let _ = writeln!(o, "info: {i}");
    }
    o
}

/// Placement table first — cutover step 2 compares it with `aerospace-auto-secondary.sh
/// --detect` — then the exact commands a pass would issue.
pub fn dry_run(r: &Resolved, cmds: &[Cmd]) -> String {
    let name = |id: u32| r.layout.iter().find(|l| l.monitor == id).map_or(String::new(), |l| l.monitor_name.clone());
    let mut o = String::from("placement (compare with aerospace-auto-secondary.sh --detect):\n");
    for p in &r.placement {
        match p.target {
            Some(t) => {
                let _ = writeln!(o, "  {:<4} monitor {t}  {}", p.workspace, name(t));
            }
            None => {
                let _ = writeln!(o, "  {:<4} (nothing satisfiable)", p.workspace);
            }
        }
    }
    o.push_str("commands:\n");
    if cmds.is_empty() {
        o.push_str("  (none — already reconciled)\n");
    }
    for c in cmds {
        let _ = writeln!(o, "  {c}");
    }
    o
}

pub fn pins(v: &[PinView]) -> String {
    if v.is_empty() {
        return "no pins\n".into();
    }
    let mut o = String::new();
    for p in v {
        let suffix = if p.suspended { "  (suspended: off its pinned monitor)" } else { "" };
        let _ = writeln!(o, "{:<4} on {}{suffix}", p.workspace, p.monitor);
    }
    o
}

pub fn status(v: &StatusView) -> String {
    let mut o = String::new();
    if v.observe {
        o.push_str("observe mode: passes are logged, nothing is executed\n");
    }
    if !v.aerospace_running {
        o.push_str("AeroSpace: not running (idle until its socket appears)\n");
    }
    let _ = writeln!(o, "profile: {}", profile_line(&v.profile, &v.profile_mode));
    match &v.last_pass {
        Some(p) => {
            let _ = writeln!(o, "last pass: {} ({}): {}", p.at, p.trigger, p.result);
        }
        None => o.push_str("last pass: none yet\n"),
    }
    if v.pins.is_empty() {
        o.push_str("pins: none\n");
    } else {
        o.push_str("pins:\n");
        for line in pins(&v.pins).lines() {
            let _ = writeln!(o, "  {line}");
        }
    }
    if v.warnings.is_empty() {
        o.push_str("warnings: none\n");
    } else {
        o.push_str("warnings:\n");
        for w in &v.warnings {
            let class = serde_json::to_value(w.class).ok().and_then(|c| c.as_str().map(String::from)).unwrap_or_default();
            let _ = writeln!(o, "  [{class}] {}  (since {})", w.message, w.since);
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;
    use crate::rules::resolve;
    use crate::testutil::desk3;

    fn resolved() -> Resolved {
        let cfg = parse(include_str!("../aerospace-responsive.example.toml")).unwrap().config;
        resolve(&cfg, &desk3(), None)
    }

    #[test]
    fn explain_shows_every_rule_and_why_it_lost() {
        let text = explain(&resolved());
        assert!(text.contains("profile: none"));
        assert!(text.contains("lost: portrait is false, wanted true"));
        assert!(text.contains("fallback: no rule matched"));
        assert!(text.contains("T    ->  monitor 2"));
    }

    #[test]
    fn dry_run_lists_placement_then_commands() {
        let r = resolved();
        let text = dry_run(&r, &[]);
        assert!(text.contains("1    monitor 1  LEN T23i-10 (2)"));
        assert!(text.contains("(none — already reconciled)"));
    }

    #[test]
    fn status_lists_pending_warnings() {
        let mut w = crate::state::Warnings::default();
        w.raise(crate::state::Class::Pass, "mismatch", "state still differs");
        let v = StatusView {
            profile: None,
            profile_mode: ProfileMode::None,
            lock: None,
            last_pass: None,
            pins: vec![],
            warnings: w.list(),
            aerospace_running: true,
            observe: false,
        };
        let text = status(&v);
        assert!(text.contains("[pass] state still differs"));
        assert!(text.contains("pins: none"));
    }
}
