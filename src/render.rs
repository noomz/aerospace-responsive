//! Human text for every read verb. The daemon renders; clients only print (D25 V2).

use crate::aero::Cmd;
use crate::config::Axis;
use crate::display::Display;
use crate::rules::{ProfileMode, Resolved};
use crate::state::Warning;
use crate::tiling::WsView;
use crate::world::axis_of;
use serde::Serialize;
use std::collections::BTreeMap;
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
    /// `None` when no exclusive-tiling rule is configured.
    pub exclusive_tiling: Option<TilingStatus>,
    /// `None` when no workspace template is configured.
    pub workspace_template: Option<TemplateStatus>,
}

/// `status`'s workspace-template line, from memory (no window read).
#[derive(Debug, Clone, Serialize)]
pub struct TemplateStatus {
    /// `live` | `inert`: whether arrivals rebuild (C24).
    pub state: String,
    /// Workspaces whose last rebuild completed (they hold a record).
    pub applied: Vec<String>,
}

impl TemplateStatus {
    /// `workspace template: live; C applied`.
    pub fn line(&self) -> String {
        let applied = if self.applied.is_empty() { "none".to_string() } else { self.applied.join(" ") };
        format!("workspace template: {}; {applied} applied", self.state)
    }
}

/// `status`'s exclusive-tiling line, from memory (no window read).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TilingStatus {
    /// `live` | `inert` | `observe`.
    pub state: String,
    /// Why, while inert (`aerospace subscribe exited 2`); the warning carries the detail.
    pub inert: Option<String>,
    pub rule_floated: BTreeMap<String, usize>,
}

impl TilingStatus {
    /// `exclusive tiling: live; rule-floated T:2`.
    pub fn line(&self) -> String {
        let counts: Vec<String> = self.rule_floated.iter().map(|(ws, n)| format!("{ws}:{n}")).collect();
        let floated = if counts.is_empty() { "none".to_string() } else { counts.join(" ") };
        format!("exclusive tiling: {}; rule-floated {floated}", self.state)
    }
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
        let notch = if s.notch > 0.0 { format!("  notch {}pt", s.notch) } else { String::new() };
        let _ = writeln!(
            o,
            "    {}x{} px  {}x{} pt at ({}, {})  rotation {}{notch}",
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
        let flat = match l.want.kind {
            Some(k) => format!("{flat}, {}", k.as_str()),
            None => flat.to_string(),
        };
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
    o.push_str(&gaps(r));
    for n in &r.no_match {
        let _ = writeln!(o, "warning: {n}");
    }
    for i in &r.info {
        let _ = writeln!(o, "info: {i}");
    }
    o
}

/// `explain`'s gaps section: the line the tool wants AeroSpace to hold, and where each
/// monitor's number came from — the rule, the notch that came off it, and what remains.
/// With no `[gaps]` block it says so, because then `outer.top` is not this tool's.
pub fn gaps(r: &Resolved) -> String {
    let Some(want) = crate::gaps::desired(&r.gaps) else {
        return "gaps: not managed (no [gaps] block): outer.top is left alone\n".to_string();
    };
    let mut o = String::new();
    let _ = writeln!(o, "gaps: outer.top = {}", want.to_toml());
    for d in &r.gaps {
        let comp = if d.notch > 0.0 { format!("  notch -{}", d.notch) } else { String::new() };
        let menu = if d.menu_bar_add > 0.0 { format!("  menu bar +{}", d.menu_bar_add) } else { String::new() };
        let _ = writeln!(o, "  monitor {:<3} {:<24} base {:>3} [{:<14}{comp}{menu}  ->  {:>3}]", d.monitor, d.monitor_name, d.base, d.source, d.top);
    }
    o
}

/// `explain`'s exclusive-tiling section (I.cli.explain). `state` = `inert: <why>` or
/// `observe`, carried in the header.
pub fn exclusive_tiling(views: &[WsView], state: Option<&str>) -> String {
    if views.is_empty() {
        return String::new();
    }
    let mut o = String::from("exclusive-tiling:");
    if let Some(st) = state {
        let _ = write!(o, " ({st})");
    }
    o.push('\n');
    for v in views {
        match &v.rule {
            Some(r) => {
                let action = serde_json::to_value(r.else_action).ok().and_then(|a| a.as_str().map(String::from)).unwrap_or_default();
                let _ = writeln!(o, "  {:<4} {:<24} [{}]", v.workspace, format!("else-action {action}"), r.else_action_source);
                let _ = writeln!(o, "       {:<24} [{}]", "tiling-allowlist", r.tiling_allowlist_source);
            }
            None => {
                let _ = writeln!(o, "  {:<4} (rule removed)", v.workspace);
            }
        }
        let app_w = v.entries.iter().map(|e| e.entry.app.len()).max().unwrap_or(0);
        let title = |e: &crate::config::Entry| e.title.as_ref().map_or(String::new(), |t| format!("title~{:?}", t.as_str()));
        let title_w = v.entries.iter().map(|e| title(&e.entry).len()).max().unwrap_or(0);
        for (i, e) in v.entries.iter().enumerate() {
            let limit = e.entry.max_tiled.map_or(String::new(), |n| format!("max-tiled {n}"));
            let count = match (e.tiled, e.entry.max_tiled) {
                (Some(_), Some(0)) | (None, _) => String::new(),
                (Some(k), Some(n)) => format!("  ({k}/{n} tiled)"),
                (Some(k), None) => format!("  ({k} tiled)"),
            };
            let line = format!("{} {:<app_w$}  {:<title_w$}  {limit}{count}", i + 1, e.entry.app, title(&e.entry));
            let _ = writeln!(o, "         {}", line.trim_end());
        }
        let _ = writeln!(o, "       converge: {}", v.converge);
        let Some(wins) = &v.windows else { continue };
        if wins.is_empty() {
            o.push_str("       windows: none\n");
            continue;
        }
        o.push_str("       windows:\n");
        let name_w = wins.iter().map(|w| w.app_name.len()).max().unwrap_or(0);
        let titles: Vec<String> = wins.iter().map(|w| format!("{:?}", w.title)).collect();
        let tw = titles.iter().map(|t| t.chars().count()).max().unwrap_or(0);
        for (w, t) in wins.iter().zip(&titles) {
            let state = if axis_of(&w.layout).is_some() { "tiled" } else { w.layout.as_str() };
            let pad = tw - t.chars().count();
            let _ = writeln!(o, "         {:<6} {:<name_w$}  {t}{:pad$}  {state:<9} {}", w.id, w.app_name, "", w.verdict);
        }
    }
    o
}

/// `explain`'s workspace-template section (I.tpl.cli.explain). `state` = `inert: <why>` or
/// `observe`, carried in the header.
pub fn workspace_template(views: &[crate::template::View], state: Option<&str>) -> String {
    use crate::template::Win;
    const APP_W: usize = 28;
    if views.is_empty() {
        return String::new();
    }
    let mut o = String::from("workspace-template:");
    if let Some(st) = state {
        let _ = write!(o, " ({st})");
    }
    o.push('\n');
    let win = |w: &Win| format!("{} {}", w.id, w.app_name).trim_end().to_string();
    for v in views {
        let when = v.when.as_ref().map_or(String::new(), |w| format!("   when {w}"));
        let on = match &v.monitor {
            Some(m) => format!("on {m}: {}", if v.applies { "applies" } else { "not applied" }),
            None => "not on a monitor".into(),
        };
        let _ = writeln!(o, "  {:<4} root {}{when}   [{}]   {on}", v.workspace, v.root, v.source);
        let _ = writeln!(o, "       record: {}", v.record);
        o.push_str("       tree:\n");
        let label = |r: &crate::template::Row| {
            let text = if r.label.chars().count() > APP_W { format!("{}…", r.label.chars().take(APP_W - 1).collect::<String>()) } else { r.label.clone() };
            format!("{}{} {text}", "  ".repeat(r.depth), r.path)
        };
        let col = v.tree.iter().filter(|r| r.slot).map(|r| label(r).chars().count()).max().unwrap_or(0);
        for r in &v.tree {
            let l = label(r);
            if !r.slot {
                let _ = writeln!(o, "         {l}");
                continue;
            }
            let pad = col - l.chars().count();
            match r.windows.split_first() {
                None => {
                    let _ = writeln!(o, "         {l}{:pad$}  (no window)", "");
                }
                Some((first, more)) => {
                    let _ = writeln!(o, "         {l}{:pad$}  {}", "", win(first));
                    for w in more {
                        let _ = writeln!(o, "         {:col$}  {}", "", win(w));
                    }
                }
            }
        }
        if v.rest.is_empty() {
            o.push_str("       rest: none\n");
        } else {
            let rest: Vec<String> = v.rest.iter().map(win).collect();
            let _ = writeln!(o, "       rest: {}", rest.join(", "));
        }
    }
    o
}

/// Placement table first — cutover step 2 compares it with `aerospace-auto-secondary.sh
/// --detect` — then the pending exclusive-tiling converges and template rebuilds with their
/// reasons, then the exact commands a pass would issue. `converge`, `rebuild` = (workspace, reason).
pub fn dry_run(r: &Resolved, cmds: &[Cmd], converge: &[(String, String)], rebuild: &[(String, String)]) -> String {
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
    if !converge.is_empty() {
        o.push_str("exclusive-tiling converge:\n");
        for (ws, why) in converge {
            let _ = writeln!(o, "  {ws:<4} {why}");
        }
    }
    if !rebuild.is_empty() {
        o.push_str("workspace-template rebuild:\n");
        for (ws, why) in rebuild {
            let _ = writeln!(o, "  {ws:<4} {why}");
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
    if let Some(t) = &v.exclusive_tiling {
        let _ = writeln!(o, "{}", t.line());
    }
    if let Some(t) = &v.workspace_template {
        let _ = writeln!(o, "{}", t.line());
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
    use std::collections::BTreeSet;

    fn resolved() -> Resolved {
        let cfg = parse(include_str!("../aerospace-responsive.example.toml")).unwrap().config;
        resolve(&cfg, &desk3(), None)
    }

    #[test]
    fn explain_prints_the_gap_arithmetic_and_the_line() {
        let cfg = parse("[gaps]\ntop-default = 1\n[[gaps.top]]\nmonitor = { builtin = true }\nvalue = 52\n").unwrap().config;
        let r = resolve(&cfg, &desk3(), None);
        let text = explain(&r);
        // The built-in (monitor 2) carries a 32 pt notch in the fixtures: 52 - 32 = 20 there, 1 on the LENs.
        assert!(text.contains("gaps: outer.top = [{ monitor.2 = 20 }, 1]"), "{text}");
        assert!(text.contains("notch -32"), "{text}");
        let unmanaged = resolve(&crate::config::Config::default(), &desk3(), None);
        assert!(explain(&unmanaged).contains("gaps: not managed"), "no [gaps] block says so");
        assert!(monitors(&[]).is_empty());
    }

    #[test]
    fn monitors_prints_the_measured_notch() {
        let d = crate::testutil::builtin(2);
        let v = MonitorView { labels: vec![], portrait: d.portrait(), external: d.external(), pixel_area: d.pixel_area(), display: d };
        let text = monitors(&[v]);
        assert!(text.contains("notch 32pt"), "{text}");
        let plain = MonitorView { labels: vec![], portrait: false, external: true, pixel_area: 0, display: crate::testutil::len_landscape(3) };
        assert!(!monitors(&[plain]).contains("notch"), "no notch, no word");
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
        let text = dry_run(&r, &[], &[], &[]);
        assert!(text.contains("1    monitor 1  LEN T23i-10 (2)"));
        assert!(text.contains("(none — already reconciled)"));
        assert!(!text.contains("exclusive-tiling"), "no block without a pending converge");
    }

    #[test]
    fn dry_run_puts_the_converge_block_before_the_commands() {
        let cmds = [Cmd::Float { id: 5230 }, Cmd::Tile { id: 4380 }];
        let text = dry_run(&resolved(), &cmds, &[("T".into(), "rule changed: tiling-allowlist (profile desk3 #1)".into())], &[]);
        let block = text.find("exclusive-tiling converge:\n  T    rule changed: tiling-allowlist (profile desk3 #1)\n").expect(&text);
        let commands = text.find("commands:\n  aerospace layout floating --window-id 5230\n  aerospace layout tiling --window-id 4380").expect(&text);
        assert!(text.find("placement").unwrap() < block && block < commands);
    }

    #[test]
    fn explain_section_matches_the_mockup() {
        use crate::state::{RuleRecord, TilingMemory};
        use crate::world::WinInfo;
        let cfg = parse(
            "[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [\n\
             { app = \"net.kovidgoyal.kitty\", title = \"Settings\", max-tiled = 0 },\n\
             { app = \"net.kovidgoyal.kitty\", max-tiled = 1 },\n]\n",
        )
        .unwrap()
        .config;
        let rules = crate::tiling::effective(&cfg, None);
        let mut mem = TilingMemory::default();
        mem.records.insert("T".into(), RuleRecord::of(&rules[0]));
        mem.insert("T", 4380, "net.kovidgoyal.kitty");
        let w = |id, app: &str, name: &str, title: &str, layout: &str| WinInfo {
            id,
            workspace: "T".into(),
            app: app.into(),
            app_name: name.into(),
            title: title.into(),
            layout: layout.into(),
        };
        let wins = [
            w(4211, "net.kovidgoyal.kitty", "kitty", "noomz: ~", "h_tiles"),
            w(4380, "net.kovidgoyal.kitty", "kitty", "noomz: htop", "floating"),
            w(5120, "com.apple.Preview", "Preview", "x.pdf", "floating"),
        ];
        let text = exclusive_tiling(&crate::tiling::view(&rules, &mem, Some(&wins)), None);
        let want = "\
exclusive-tiling:
  T    else-action float        [default]
       tiling-allowlist         [top-level #1]
         1 net.kovidgoyal.kitty  title~\"Settings\"  max-tiled 0
         2 net.kovidgoyal.kitty                    max-tiled 1  (1/1 tiled)
       converge: up to date
       windows:
         4211   kitty    \"noomz: ~\"     tiled     entry 2 (1/1)
         4380   kitty    \"noomz: htop\"  floating  rule-floated: entry 2 full
         5120   Preview  \"x.pdf\"        floating  yours (rule ignores)
";
        assert_eq!(text, want, "\n{text}");
        let inert = exclusive_tiling(&crate::tiling::view(&rules, &mem, None), Some("inert: aerospace subscribe exited 2"));
        assert!(inert.starts_with("exclusive-tiling: (inert: aerospace subscribe exited 2)\n"), "{inert}");
        assert!(!inert.contains("windows:"));
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
            exclusive_tiling: None,
            workspace_template: None,
        };
        let text = status(&v);
        assert!(text.contains("[pass] state still differs"));
        assert!(text.contains("pins: none"));
        assert!(!text.contains("exclusive tiling"), "omitted when no rule is configured");
        let t = TilingStatus { state: "live".into(), inert: None, rule_floated: BTreeMap::from([("T".to_string(), 2)]) };
        let text = status(&StatusView { exclusive_tiling: Some(t), ..v.clone() });
        assert!(text.contains("exclusive tiling: live; rule-floated T:2\n"), "{text}");
        assert!(!text.contains("workspace template"), "omitted when no template is configured");
        let t = TemplateStatus { state: "inert".into(), applied: vec![] };
        assert!(status(&StatusView { workspace_template: Some(t), ..v.clone() }).contains("workspace template: inert; none applied\n"));
        let t = TemplateStatus { state: "live".into(), applied: vec!["C".into(), "D".into()] };
        assert!(status(&StatusView { workspace_template: Some(t), ..v }).contains("workspace template: live; C D applied\n"));
    }

    #[test]
    fn dry_run_puts_the_rebuild_block_after_the_converge_block() {
        let text = dry_run(&resolved(), &[Cmd::Flatten { ws: "C".into() }], &[("T".into(), "first run".into())], &[("C".into(), "first run".into())]);
        let converge = text.find("exclusive-tiling converge:\n").expect(&text);
        let rebuild = text.find("workspace-template rebuild:\n  C    first run\n").expect(&text);
        let commands = text.find("commands:\n  aerospace flatten-workspace-tree --workspace C").expect(&text);
        assert!(converge < rebuild && rebuild < commands, "{text}");
    }

    #[test]
    fn workspace_template_section_matches_the_mockup() {
        use crate::state::TemplateRecords;
        use crate::world::WinInfo;
        let cfg = parse(
            "[[workspace-template]]\nworkspaces = [\"C\"]\nwhen = { builtin = true }\nroot = \"h\"\n\
             tree = [{ app = \"jp.naver.line.mac\" }, [{ app = \"com.igcl.app\" }, \
             { app = \"com.google.Chrome.app.bbdeiblfgdokhlblpgeaokenkfknecgl\" }, { app = \"x\", title = \"Inbox\" }]]\n",
        )
        .unwrap()
        .config;
        let ds = desk3();
        let r = resolve(&cfg, &ds, None);
        let w = |id, app: &str, name: &str, layout: &str| WinInfo {
            id,
            workspace: "C".into(),
            app: app.into(),
            app_name: name.into(),
            title: String::new(),
            layout: layout.into(),
        };
        let wins = [
            w(44, "jp.naver.line.mac", "LINE", "h_tiles"),
            w(10, "com.igcl.app", "IGCL", "v_tiles"),
            w(49, "com.google.Chrome.app.bbdeiblfgdokhlblpgeaokenkfknecgl", "Messenger", "v_tiles"),
            w(50, "com.google.Chrome.app.bbdeiblfgdokhlblpgeaokenkfknecgl", "Messenger", "v_tiles"),
            w(12, "com.apple.finder", "Finder", "h_tiles"),
            w(13, "com.apple.Preview", "Preview", "floating"),
        ];
        let mut records = TemplateRecords::new();
        let t = &r.templates[0].template;
        records.insert("C".into(), crate::template::wanted(t, &wins));
        let on = |m: u32| BTreeMap::from([("C".to_string(), m)]);
        let none = BTreeSet::new();
        let text = workspace_template(&crate::template::view(&r.templates, &records, &none, &on(2), &ds, &wins), None);
        let want = "\
workspace-template:
  C    root h   when builtin   [top-level #1]   on Built-in Retina Display: applies
       record: up to date
       tree:
         1 jp.naver.line.mac                 44 LINE
         2 v
           2.1 com.igcl.app                  10 IGCL
           2.2 com.google.Chrome.app.bbdei…  49 Messenger
                                             50 Messenger
           2.3 x title~\"Inbox\"               (no window)
       rest: 12 Finder
";
        assert_eq!(text, want, "\n{text}");
        // Off its monitor, pinned, or after an edit: the record line says which.
        let views = |m, pinned: &BTreeSet<String>, recs: &TemplateRecords| crate::template::view(&r.templates, recs, pinned, &on(m), &ds, &wins);
        let off = workspace_template(&views(3, &none, &records), Some("inert: aerospace subscribe exited 2"));
        assert!(off.starts_with("workspace-template: (inert: aerospace subscribe exited 2)\n"), "{off}");
        assert!(off.contains("on LEN T23i-10 (1): not applied\n       record: not applied: when does not match LEN T23i-10 (1)\n"), "{off}");
        let pinned = BTreeSet::from(["C".to_string()]);
        assert_eq!(views(2, &pinned, &records)[0].record, "not applied: pinned");
        assert_eq!(views(2, &none, &TemplateRecords::new())[0].record, "pending: first run");
        let mut stale = records.clone();
        stale.get_mut("C").unwrap().rest.clear();
        assert_eq!(views(2, &none, &stale)[0].record, "pending: windows changed");
        stale.get_mut("C").unwrap().fingerprint.push('!');
        assert_eq!(views(2, &none, &stale)[0].record, "pending: template changed");
    }
}
