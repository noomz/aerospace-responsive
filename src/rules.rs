//! Predicate matching, profile selection and rule resolution (tickets 01, 02, 06).
//! Pure: the same code answers a live pass, `explain`, and `reconcile --dry-run`.

use crate::config::{Axis, Config, LayoutRule, MonitorSpec, PlacementRule, Pred, Profile, Rank, SetPred};
use crate::display::Display;
use serde::Serialize;

/// Why `p` rejects `d`, or `None` when every present field holds.
pub fn mismatch(p: &Pred, d: &Display, cfg: &Config) -> Option<String> {
    if let Some(label) = &p.label {
        let inner = cfg.label(label).expect("labels are validated at load");
        if let Some(why) = mismatch(inner, d, cfg) {
            return Some(format!("@{label}: {why}"));
        }
    }
    macro_rules! want {
        ($field:ident, $actual:expr) => {
            if let Some(wanted) = &p.$field {
                let actual = $actual;
                if *wanted != actual {
                    return Some(format!("{} is {:?}, wanted {:?}", stringify!($field), actual, wanted));
                }
            }
        };
    }
    want!(name, d.screen.name.clone());
    want!(builtin, d.builtin());
    want!(external, d.external());
    want!(portrait, d.portrait());
    want!(landscape, d.landscape());
    want!(main, d.screen.main);
    want!(vendor, d.screen.vendor);
    want!(model, d.screen.model);
    want!(serial, d.screen.serial);
    None
}

pub fn matches(p: &Pred, d: &Display, cfg: &Config) -> bool {
    mismatch(p, d, cfg).is_none()
}

/// Why the attached set fails a profile predicate (plan P-5), or `None`.
pub fn set_mismatch(sp: &SetPred, ds: &[Display], cfg: &Config) -> Option<String> {
    if let Some(n) = sp.count
        && ds.len() != n
    {
        return Some(format!("count is {}, wanted {n}", ds.len()));
    }
    for d in ds {
        if let Some(why) = mismatch(&sp.all, d, cfg) {
            return Some(format!("{}: {why}", d.aero_name));
        }
    }
    if !assign(&sp.includes, ds, cfg, &mut vec![false; ds.len()]) {
        return Some("`includes` is not satisfied by distinct monitors".into());
    }
    None
}

/// Backtracking: each included predicate claims a distinct monitor. n is a handful.
fn assign(preds: &[Pred], ds: &[Display], cfg: &Config, used: &mut Vec<bool>) -> bool {
    let Some((first, rest)) = preds.split_first() else { return true };
    for i in 0..ds.len() {
        if !used[i] && matches(first, &ds[i], cfg) {
            used[i] = true;
            if assign(rest, ds, cfg, used) {
                return true;
            }
            used[i] = false;
        }
    }
    false
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileMode {
    Auto,
    Locked,
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileTrace {
    pub name: String,
    pub matched: bool,
    pub why_not: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Want {
    pub axis: Axis,
    pub flatten: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Considered {
    pub rule: String,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LayoutDecision {
    pub monitor: u32,
    pub monitor_name: String,
    pub want: Want,
    pub source: String,
    pub considered: Vec<Considered>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlacementDecision {
    pub workspace: String,
    /// `None` = no rule satisfiable: leave the workspace where it is (06 S3).
    pub target: Option<u32>,
    pub source: Option<String>,
    pub considered: Vec<Considered>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub profile: Option<String>,
    pub profile_mode: ProfileMode,
    pub profiles: Vec<ProfileTrace>,
    /// The lock named a profile that no longer matches; the caller clears it (02 M1).
    pub lock_released: bool,
    /// One per attached display, in monitor-id order.
    pub layout: Vec<LayoutDecision>,
    /// One per managed workspace.
    pub placement: Vec<PlacementDecision>,
    /// Layout rules matching no attached monitor (01 R4 warning).
    pub no_match: Vec<String>,
    /// Rules naming only a known-but-absent labelled monitor (06 S4: info).
    pub info: Vec<String>,
}

impl Resolved {
    pub fn want_for(&self, monitor: u32) -> Option<Want> {
        self.layout.iter().find(|l| l.monitor == monitor).map(|l| l.want)
    }
    pub fn target_for(&self, ws: &str) -> Option<u32> {
        self.placement.iter().find(|p| p.workspace == ws).and_then(|p| p.target)
    }
}

const FALLBACK: &str = "fallback: no rule matched; orientation from aspect ratio, never flatten";

pub fn resolve(cfg: &Config, ds: &[Display], lock: Option<&str>) -> Resolved {
    let (profile, profile_mode, profiles, lock_released) = choose_profile(cfg, ds, lock);
    let layout_rules = named(profile, |p| &p.layout, &cfg.layout, "layout");
    let placement_rules = named(profile, |p| &p.placement, &cfg.placement, "placement");
    let layout = ds.iter().map(|d| decide_layout(cfg, d, &layout_rules)).collect();
    let placement = cfg
        .managed_workspaces()
        .into_iter()
        .map(|ws| decide_placement(cfg, ds, ws, &placement_rules))
        .collect();
    let (mut no_match, mut info) = (vec![], vec![]);
    for (name, r) in &layout_rules {
        if ds.iter().any(|d| matches(&r.when, d, cfg)) {
            continue;
        }
        match label_only(&r.when) {
            Some(l) => info.push(format!("{name}: monitor @{l} is not attached")),
            None => no_match.push(format!("{name} matches no attached monitor")),
        }
    }
    Resolved {
        profile: profile.map(|p| p.name.clone()),
        profile_mode,
        profiles,
        lock_released,
        layout,
        placement,
        no_match,
        info,
    }
}

fn choose_profile<'c>(cfg: &'c Config, ds: &[Display], lock: Option<&str>) -> (Option<&'c Profile>, ProfileMode, Vec<ProfileTrace>, bool) {
    let traces: Vec<ProfileTrace> = cfg
        .profile
        .iter()
        .map(|p| {
            let why_not = set_mismatch(&p.when, ds, cfg);
            ProfileTrace { name: p.name.clone(), matched: why_not.is_none(), why_not }
        })
        .collect();
    let mut released = false;
    if let Some(l) = lock {
        if traces.iter().any(|t| t.name == l && t.matched) {
            let p = cfg.profile.iter().find(|p| p.name == l);
            return (p, ProfileMode::Locked, traces, false);
        }
        released = true;
    }
    match cfg.profile.iter().zip(&traces).find(|(_, t)| t.matched) {
        Some((p, _)) => (Some(p), ProfileMode::Auto, traces, released),
        None => (None, ProfileMode::None, traces, released),
    }
}

/// Profile rules first, then base rules (plan P-4), each with a readable name.
fn named<'c, R>(profile: Option<&'c Profile>, pick: impl Fn(&'c Profile) -> &'c Vec<R>, base: &'c [R], kind: &str) -> Vec<(String, &'c R)> {
    let mut out = vec![];
    if let Some(p) = profile {
        for (i, r) in pick(p).iter().enumerate() {
            out.push((format!("profile {:?} {kind}[{i}]", p.name), r));
        }
    }
    for (i, r) in base.iter().enumerate() {
        out.push((format!("{kind}[{i}]"), r));
    }
    out
}

fn label_only(p: &Pred) -> Option<&str> {
    let l = p.label.as_deref()?;
    (Pred { label: None, ..p.clone() } == Pred::default()).then_some(l)
}

fn decide_layout(cfg: &Config, d: &Display, rules: &[(String, &LayoutRule)]) -> LayoutDecision {
    let mut considered = vec![];
    let mut won: Option<(String, Want)> = None;
    for (name, r) in rules {
        let outcome = if won.is_some() {
            "skipped: an earlier rule won".to_string()
        } else {
            match mismatch(&r.when, d, cfg) {
                None => {
                    won = Some((name.clone(), Want { axis: r.orientation, flatten: r.flatten }));
                    "won".to_string()
                }
                Some(why) => format!("lost: {why}"),
            }
        };
        considered.push(Considered { rule: name.clone(), outcome });
    }
    let (source, want) = won.unwrap_or_else(|| {
        let axis = if d.portrait() { Axis::V } else { Axis::H };
        (FALLBACK.to_string(), Want { axis, flatten: false })
    });
    LayoutDecision { monitor: d.aero_id, monitor_name: d.aero_name.clone(), want, source, considered }
}

/// Exactly one monitor per placement rule; ties break by monitor-id (01 R6).
fn select(spec: &MonitorSpec, ds: &[Display], cfg: &Config) -> Result<u32, String> {
    match spec {
        MonitorSpec::Pred(p) => {
            let (mut hits, mut whys) = (vec![], vec![]);
            for d in ds {
                match mismatch(p, d, cfg) {
                    None => hits.push(d.aero_id),
                    Some(w) => whys.push(format!("{}: {w}", d.aero_name)),
                }
            }
            hits.into_iter().min().ok_or_else(|| if whys.is_empty() { "no monitor attached".into() } else { whys.join("; ") })
        }
        MonitorSpec::Rank(rank) => ds
            .iter()
            .min_by_key(|d| {
                let area = d.pixel_area() as i128;
                (if *rank == Rank::MinPixelArea { area } else { -area }, d.aero_id)
            })
            .map(|d| d.aero_id)
            .ok_or_else(|| "no monitor attached".into()),
    }
}

fn decide_placement(cfg: &Config, ds: &[Display], ws: String, rules: &[(String, &PlacementRule)]) -> PlacementDecision {
    let mut considered = vec![];
    let (mut target, mut source) = (None, None);
    for (name, r) in rules.iter().filter(|(_, r)| r.workspaces.contains(&ws)) {
        let outcome = if target.is_some() {
            "skipped: an earlier rule was satisfiable".to_string()
        } else {
            match select(&r.monitor, ds, cfg) {
                Ok(id) => {
                    target = Some(id);
                    source = Some(name.clone());
                    format!("won: monitor {id}")
                }
                Err(why) => format!("unsatisfiable: {why}"),
            }
        };
        considered.push(Considered { rule: name.clone(), outcome });
    }
    PlacementDecision { workspace: ws, target, source, considered }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Axis, parse};
    use crate::testutil::{builtin, desk3, len_landscape, len_portrait};

    fn example() -> Config {
        parse(include_str!("../aerospace-responsive.example.toml")).unwrap().config
    }

    #[test]
    fn desk3_layout() {
        let r = resolve(&example(), &desk3(), None);
        assert_eq!(r.want_for(1), Some(Want { axis: Axis::V, flatten: true }));
        assert_eq!(r.want_for(2), Some(Want { axis: Axis::H, flatten: false }));
        let landscape = r.layout.iter().find(|l| l.monitor == 3).unwrap();
        assert_eq!(landscape.want, Want { axis: Axis::H, flatten: false });
        assert!(landscape.source.starts_with("fallback"));
    }

    #[test]
    fn desk3_placement_translates_the_old_script() {
        let r = resolve(&example(), &desk3(), None);
        assert_eq!(r.target_for("1"), Some(1));
        for ws in ["2", "3", "C"] {
            assert_eq!(r.target_for(ws), Some(3), "{ws}");
        }
        assert_eq!(r.target_for("T"), Some(2));
        assert!(r.no_match.is_empty());
        assert_eq!(r.profile, None); // desk wants exactly 2 monitors
    }

    #[test]
    fn laptop_only() {
        let r = resolve(&example(), &[builtin(1)], None);
        assert_eq!(r.profile.as_deref(), Some("laptop-only"));
        assert_eq!(r.profile_mode, ProfileMode::Auto);
        for ws in ["1", "2", "3", "C"] {
            assert_eq!(r.target_for(ws), None, "{ws}");
        }
        assert_eq!(r.target_for("T"), Some(1));
        assert_eq!(r.no_match, ["layout[0] matches no attached monitor"]);
    }

    #[test]
    fn placement_falls_through_to_the_next_satisfiable_rule() {
        let ds = [len_portrait(1), builtin(2)];
        let r = resolve(&example(), &ds, None);
        assert_eq!(r.profile.as_deref(), Some("desk"));
        assert_eq!(r.target_for("2"), Some(1));
        let p = r.placement.iter().find(|p| p.workspace == "2").unwrap();
        assert!(p.considered[0].outcome.starts_with("unsatisfiable"));
        assert_eq!(p.considered[1].outcome, "won: monitor 1");
    }

    #[test]
    fn explain_records_why_each_rule_lost() {
        let r = resolve(&example(), &desk3(), None);
        let b = r.layout.iter().find(|l| l.monitor == 2).unwrap();
        assert_eq!(b.considered[0].outcome, "lost: portrait is false, wanted true");
        assert_eq!(b.considered[1].outcome, "won");
        let p = r.layout.iter().find(|l| l.monitor == 1).unwrap();
        assert_eq!(p.considered[1].outcome, "skipped: an earlier rule won");
    }

    #[test]
    fn ranking_uses_native_pixels() {
        let cfg = parse("[[placement]]\nworkspaces = [\"X\"]\nmonitor = \"min(pixel-area)\"\n").unwrap().config;
        let r = resolve(&cfg, &[builtin(2), len_landscape(3)], None);
        assert_eq!(r.target_for("X"), Some(3)); // points would pick the built-in (01 R2)
    }

    #[test]
    fn profile_rules_are_consulted_first() {
        let cfg = parse(
            "[[layout]]\nwhen = { builtin = true }\norientation = \"h\"\nflatten = false\n\
             [[profile]]\nname = \"p\"\nwhen = {}\n\
             [[profile.layout]]\nwhen = { builtin = true }\norientation = \"v\"\nflatten = false\n",
        )
        .unwrap()
        .config;
        let r = resolve(&cfg, &[builtin(1)], None);
        assert_eq!(r.want_for(1).unwrap().axis, Axis::V);
        assert_eq!(r.layout[0].source, "profile \"p\" layout[0]");
    }

    #[test]
    fn lock_selects_a_matching_profile_and_auto_releases() {
        let cfg = parse("[[profile]]\nname = \"a\"\nwhen = {}\n[[profile]]\nname = \"b\"\nwhen = { count = 1 }\n").unwrap().config;
        let one = [builtin(1)];
        assert_eq!(resolve(&cfg, &one, None).profile.as_deref(), Some("a"));
        let locked = resolve(&cfg, &one, Some("b"));
        assert_eq!((locked.profile.as_deref(), &locked.profile_mode, locked.lock_released), (Some("b"), &ProfileMode::Locked, false));
        let two = [builtin(1), len_landscape(2)];
        let released = resolve(&cfg, &two, Some("b"));
        assert!(released.lock_released);
        assert_eq!(released.profile.as_deref(), Some("a"));
    }

    #[test]
    fn includes_needs_distinct_monitors() {
        let cfg = example();
        let sp = SetPred { count: None, includes: vec![Pred { external: Some(true), ..Pred::default() }; 2], all: Pred::default() };
        assert!(set_mismatch(&sp, &[len_portrait(1), builtin(2)], &cfg).is_some());
        assert!(set_mismatch(&sp, &desk3(), &cfg).is_none());
    }

    #[test]
    fn absent_labelled_monitor_is_info_not_a_warning() {
        let cfg = parse(
            "[[monitor]]\nlabel = \"spare\"\nmatch = { name = \"LEN T2364pA\" }\n\
             [[layout]]\nwhen = { label = \"spare\" }\norientation = \"v\"\nflatten = true\n",
        )
        .unwrap()
        .config;
        let r = resolve(&cfg, &desk3(), None);
        assert!(r.no_match.is_empty());
        assert_eq!(r.info, ["layout[0]: monitor @spare is not attached"]);
    }
}
