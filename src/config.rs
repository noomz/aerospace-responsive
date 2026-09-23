//! `aerospace-responsive.toml` schema (ticket 06), parse and load-time validation.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// = AeroSpace's `screenSleepWakeSettleDelay` (ticket 07, 06 S5).
pub const TRAILING_FLOOR_MS: u64 = 1000;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub monitor: Vec<MonitorLabel>,
    #[serde(default)]
    pub layout: Vec<LayoutRule>,
    #[serde(default)]
    pub placement: Vec<PlacementRule>,
    #[serde(default)]
    pub profile: Vec<Profile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Settings {
    #[serde(default = "yes")]
    pub warn_on_no_match: bool,
    #[serde(default)]
    pub debounce: Debounce,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { warn_on_no_match: true, debounce: Debounce::default() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Debounce {
    #[serde(default = "floor")]
    pub trailing_ms: u64,
}

impl Default for Debounce {
    fn default() -> Self {
        Debounce { trailing_ms: TRAILING_FLOOR_MS }
    }
}

fn yes() -> bool {
    true
}

fn floor() -> u64 {
    TRAILING_FLOOR_MS
}

/// Per-monitor predicate: every present field must hold (01 R5 property set).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pred {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portrait: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landscape: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorLabel {
    pub label: String,
    #[serde(rename = "match")]
    pub matches: Pred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Axis {
    #[serde(rename = "h")]
    H,
    #[serde(rename = "v")]
    V,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutRule {
    pub when: Pred,
    pub orientation: Axis,
    pub flatten: bool,
}

/// A placement target: a predicate, or the one ranking string form (06 S1).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum MonitorSpec {
    Rank(Rank),
    Pred(Pred),
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub enum Rank {
    MinPixelArea,
    MaxPixelArea,
}

impl TryFrom<String> for Rank {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        match s.as_str() {
            "min(pixel-area)" => Ok(Rank::MinPixelArea),
            "max(pixel-area)" => Ok(Rank::MaxPixelArea),
            other => Err(format!("unknown ranking {other:?}; expected \"min(pixel-area)\" or \"max(pixel-area)\"")),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementRule {
    pub workspaces: Vec<String>,
    pub monitor: MonitorSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub when: SetPred,
    #[serde(default)]
    pub layout: Vec<LayoutRule>,
    #[serde(default)]
    pub placement: Vec<PlacementRule>,
}

/// Predicate over the attached SET (02 M1, plan P-5): `count` is exact, `includes` needs a
/// distinct matching monitor per entry, every other key must hold for ALL attached monitors.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(try_from = "toml::Table")]
pub struct SetPred {
    pub count: Option<usize>,
    pub includes: Vec<Pred>,
    pub all: Pred,
}

impl TryFrom<toml::Table> for SetPred {
    type Error = String;
    fn try_from(mut t: toml::Table) -> Result<Self, String> {
        let count = match t.remove("count") {
            None => None,
            Some(v) => Some(
                v.as_integer()
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| "`count` must be a non-negative integer".to_string())? as usize,
            ),
        };
        let includes = match t.remove("includes") {
            None => vec![],
            Some(v) => Vec::<Pred>::deserialize(v).map_err(|e| format!("`includes`: {e}"))?,
        };
        let all = Pred::deserialize(toml::Value::Table(t)).map_err(|e| e.to_string())?;
        Ok(SetPred { count, includes, all })
    }
}

#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// Non-fatal problems (the trailing-ms floor). Load errors are `Err`.
    pub warnings: Vec<String>,
}

pub fn load(path: &Path) -> Result<Loaded> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).with_context(|| format!("in {}", path.display()))
}

pub fn parse(text: &str) -> Result<Loaded> {
    let mut config: Config = toml::from_str(text).context("parse error")?;
    let mut warnings = vec![];
    if config.settings.debounce.trailing_ms < TRAILING_FLOOR_MS {
        warnings.push(format!(
            "settings.debounce.trailing-ms = {} is below the {TRAILING_FLOOR_MS} ms floor; using {TRAILING_FLOOR_MS}",
            config.settings.debounce.trailing_ms
        ));
        config.settings.debounce.trailing_ms = TRAILING_FLOOR_MS;
    }
    validate(&config)?;
    Ok(Loaded { config, warnings })
}

fn validate(c: &Config) -> Result<()> {
    let mut labels = BTreeSet::new();
    for m in &c.monitor {
        if m.matches.label.is_some() {
            bail!("[[monitor]] {:?}: `match` cannot reference another label", m.label);
        }
        if !labels.insert(m.label.as_str()) {
            bail!("duplicate [[monitor]] label {:?}", m.label);
        }
    }
    let check = |p: &Pred, at: &str| -> Result<()> {
        match &p.label {
            Some(l) if !labels.contains(l.as_str()) => bail!("{at}: label {l:?} has no [[monitor]] block"),
            _ => Ok(()),
        }
    };
    let check_rules = |layout: &[LayoutRule], placement: &[PlacementRule], at: &str| -> Result<()> {
        for (i, r) in layout.iter().enumerate() {
            check(&r.when, &format!("{at}layout[{i}]"))?;
        }
        for (i, r) in placement.iter().enumerate() {
            if let MonitorSpec::Pred(p) = &r.monitor {
                check(p, &format!("{at}placement[{i}]"))?;
            }
        }
        Ok(())
    };
    check_rules(&c.layout, &c.placement, "")?;
    let mut names = BTreeSet::new();
    for p in &c.profile {
        if !names.insert(p.name.as_str()) {
            bail!("duplicate profile name {:?}", p.name);
        }
        let at = format!("profile {:?} ", p.name);
        check(&p.when.all, &format!("{at}when"))?;
        for inc in &p.when.includes {
            check(inc, &format!("{at}when.includes"))?;
        }
        check_rules(&p.layout, &p.placement, &at)?;
    }
    Ok(())
}

impl Config {
    pub fn label(&self, name: &str) -> Option<&Pred> {
        self.monitor.iter().find(|m| m.label == name).map(|m| &m.matches)
    }

    /// Every workspace named by any placement list, base or profile. This is the opt-in
    /// placement set (D10) and what `install` adds to `persistent-workspaces` (D20).
    pub fn managed_workspaces(&self) -> BTreeSet<String> {
        self.placement
            .iter()
            .chain(self.profile.iter().flat_map(|p| &p.placement))
            .flat_map(|r| r.workspaces.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../aerospace-responsive.example.toml");

    #[test]
    fn parses_the_example_config() {
        let l = parse(EXAMPLE).unwrap();
        assert!(l.warnings.is_empty());
        assert_eq!(l.config.layout.len(), 2);
        assert_eq!(l.config.placement.len(), 5);
        assert_eq!(l.config.profile.len(), 2);
        assert_eq!(l.config.layout[0].orientation, Axis::V);
        assert!(l.config.layout[0].flatten);
        let managed: Vec<String> = l.config.managed_workspaces().into_iter().collect();
        assert_eq!(managed, ["1", "2", "3", "C", "T"]);
    }

    #[test]
    fn profile_when_splits_set_keys_from_per_monitor_keys() {
        let l = parse(EXAMPLE).unwrap();
        let desk = &l.config.profile[0].when;
        assert_eq!(desk.count, Some(2));
        assert_eq!(desk.includes.len(), 2);
        assert_eq!(desk.includes[0].label.as_deref(), Some("portrait-lenovo"));
        assert_eq!(desk.all, Pred::default());
        let laptop = &l.config.profile[1].when;
        assert_eq!(laptop.count, Some(1));
        assert_eq!(laptop.all.builtin, Some(true));
    }

    #[test]
    fn unknown_label_is_a_load_error() {
        let err = parse("[[layout]]\nwhen = { label = \"nope\" }\norientation = \"v\"\nflatten = true\n").unwrap_err();
        assert!(format!("{err:#}").contains("label \"nope\" has no [[monitor]] block"), "{err:#}");
    }

    #[test]
    fn label_block_cannot_reference_a_label() {
        let err = parse("[[monitor]]\nlabel = \"a\"\nmatch = { label = \"a\" }\n").unwrap_err();
        assert!(format!("{err:#}").contains("cannot reference another label"));
    }

    #[test]
    fn trailing_below_floor_is_clamped_with_a_warning() {
        let l = parse("[settings.debounce]\ntrailing-ms = 200\n").unwrap();
        assert_eq!(l.config.settings.debounce.trailing_ms, TRAILING_FLOOR_MS);
        assert_eq!(l.warnings.len(), 1);
    }

    #[test]
    fn unknown_keys_are_parse_errors() {
        assert!(parse("[settings.debounce]\nsettle-ms = 250\n").is_err());
        assert!(parse("[[layout]]\nwhen = { portrait = true, portrat = true }\norientation = \"v\"\nflatten = true\n").is_err());
    }

    #[test]
    fn placement_monitor_accepts_a_ranking_string() {
        let l = parse("[[placement]]\nworkspaces = [\"X\"]\nmonitor = \"min(pixel-area)\"\n").unwrap();
        assert_eq!(l.config.placement[0].monitor, MonitorSpec::Rank(Rank::MinPixelArea));
        assert!(parse("[[placement]]\nworkspaces = [\"X\"]\nmonitor = \"median(x)\"\n").is_err());
    }

    #[test]
    fn duplicate_profile_names_are_rejected() {
        let text = "[[profile]]\nname = \"a\"\nwhen = {}\n[[profile]]\nname = \"a\"\nwhen = {}\n";
        assert!(parse(text).is_err());
    }

    #[test]
    fn empty_file_is_a_valid_empty_config() {
        let l = parse("").unwrap();
        assert!(l.config.layout.is_empty() && l.config.placement.is_empty());
        assert_eq!(l.config.settings.debounce.trailing_ms, 1000);
        assert!(l.config.settings.warn_on_no_match);
    }
}
