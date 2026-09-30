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
    #[serde(default, rename = "exclusive-tiling")]
    pub exclusive_tiling: Vec<TilingBlock>,
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
    /// 1-based ordinal, in monitor-id order, among the monitors matching the other fields.
    /// A set-level property: only valid in a `[[placement]]` monitor (validated at load).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nth: Option<u32>,
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
    #[serde(default, rename = "exclusive-tiling")]
    pub exclusive_tiling: Vec<TilingBlock>,
}

/// One `[[exclusive-tiling]]` block. Blocks cascade per workspace and per field, last wins
/// (top-level in file order, then the active profile's); see `tiling::cascade`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct TilingBlock {
    pub workspaces: Vec<String>,
    #[serde(default)]
    pub else_action: Option<ElseAction>,
    /// Replaced whole by a later block, never concatenated.
    #[serde(default)]
    pub tiling_allowlist: Option<Vec<Entry>>,
}

/// What exclusive tiling does to a window it does not admit. v1 has one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ElseAction {
    #[default]
    Float,
}

/// A tiling allowlist entry: exact bundle id, optional title regex, optional tile limit
/// (absent = unlimited, 0 = carve-out: matches, never tiles).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Entry {
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<TitleRegex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tiled: Option<u32>,
}

/// AeroSpace's `window-title-regex-substring` semantics: case-insensitive, unanchored, so
/// patterns copy over from `aerospace.toml`. Compared by pattern text.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct TitleRegex {
    pattern: String,
    re: regex::Regex,
}

impl TitleRegex {
    pub fn as_str(&self) -> &str {
        &self.pattern
    }
    pub fn is_match(&self, title: &str) -> bool {
        self.re.is_match(title)
    }
}

impl TryFrom<String> for TitleRegex {
    type Error = String;
    fn try_from(pattern: String) -> Result<Self, String> {
        let re = regex::RegexBuilder::new(&pattern).case_insensitive(true).build().map_err(|e| format!("invalid title regex: {e}"))?;
        Ok(TitleRegex { pattern, re })
    }
}

impl From<TitleRegex> for String {
    fn from(t: TitleRegex) -> String {
        t.pattern
    }
}

impl PartialEq for TitleRegex {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
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
    for m in &c.monitor {
        if m.matches.nth.is_some() {
            bail!("[[monitor]] {:?}: nth is only valid in [[placement]] monitor", m.label);
        }
    }
    let check = |p: &Pred, at: &str| -> Result<()> {
        match &p.label {
            Some(l) if !labels.contains(l.as_str()) => bail!("{at}: label {l:?} has no [[monitor]] block"),
            _ => Ok(()),
        }
    };
    // Per-monitor predicates: an ordinal over a set of matches means nothing here.
    let check_per_monitor = |p: &Pred, at: &str| -> Result<()> {
        if p.nth.is_some() {
            bail!("{at}: nth is only valid in [[placement]] monitor");
        }
        check(p, at)
    };
    let check_rules = |layout: &[LayoutRule], placement: &[PlacementRule], at: &str| -> Result<()> {
        for (i, r) in layout.iter().enumerate() {
            check_per_monitor(&r.when, &format!("{at}layout[{i}]"))?;
        }
        for (i, r) in placement.iter().enumerate() {
            if let MonitorSpec::Pred(p) = &r.monitor {
                let at = format!("{at}placement[{i}]");
                if p.nth == Some(0) {
                    bail!("{at}: nth = 0 is invalid; nth is 1-based");
                }
                check(p, &at)?;
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
        check_per_monitor(&p.when.all, &format!("{at}when"))?;
        for inc in &p.when.includes {
            check_per_monitor(inc, &format!("{at}when.includes"))?;
        }
        check_rules(&p.layout, &p.placement, &at)?;
    }
    validate_tiling(c)
}

fn validate_tiling(c: &Config) -> Result<()> {
    let top = c.exclusive_tiling.iter().enumerate().map(|(i, b)| (format!("top-level #{}", i + 1), b));
    let prof = c.profile.iter().flat_map(|p| p.exclusive_tiling.iter().enumerate().map(|(i, b)| (format!("profile {} #{}", p.name, i + 1), b)));
    for (at, b) in top.chain(prof) {
        if b.workspaces.is_empty() {
            bail!("[[exclusive-tiling]] {at}: `workspaces` is empty");
        }
    }
    // A rule must say what may tile wherever it applies: top-level alone, and under each profile.
    let setups = std::iter::once(None).chain(c.profile.iter().map(Some));
    for p in setups {
        for (ws, f) in crate::tiling::cascade(c, p) {
            if f.tiling_allowlist.is_none() {
                let under = p.map_or("top-level".to_string(), |p| format!("profile {:?}", p.name));
                bail!("[[exclusive-tiling]] workspace {ws:?} has no tiling-allowlist ({under})");
            }
        }
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

    /// Any `[[exclusive-tiling]]` block anywhere: the subscribe child runs only then (V14).
    pub fn has_exclusive_tiling(&self) -> bool {
        !self.exclusive_tiling.is_empty() || self.profile.iter().any(|p| !p.exclusive_tiling.is_empty())
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
    fn nth_zero_is_a_load_error() {
        let err = parse("[[placement]]\nworkspaces = [\"X\"]\nmonitor = { main = false, nth = 0 }\n").unwrap_err();
        assert!(format!("{err:#}").contains("1-based"), "{err:#}");
    }

    #[test]
    fn nth_outside_placement_is_a_load_error() {
        let only = "nth is only valid in [[placement]] monitor";
        let layout = parse("[[layout]]\nwhen = { portrait = true, nth = 1 }\norientation = \"v\"\nflatten = true\n").unwrap_err();
        assert!(format!("{layout:#}").contains(only), "{layout:#}");
        let label = parse("[[monitor]]\nlabel = \"a\"\nmatch = { builtin = false, nth = 1 }\n").unwrap_err();
        assert!(format!("{label:#}").contains(only), "{label:#}");
        let includes = parse("[[profile]]\nname = \"p\"\nwhen = { includes = [{ nth = 2 }] }\n").unwrap_err();
        assert!(format!("{includes:#}").contains(only), "{includes:#}");
        let all = parse("[[profile]]\nname = \"p\"\nwhen = { nth = 1 }\n").unwrap_err();
        assert!(format!("{all:#}").contains(only), "{all:#}");
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

    fn tiling_err(text: &str) -> String {
        format!("{:#}", parse(text).unwrap_err())
    }

    #[test]
    fn exclusive_tiling_load_errors() {
        let kitty = "tiling-allowlist = [{ app = \"net.kovidgoyal.kitty\" }]\n";
        assert!(tiling_err(&format!("[[exclusive-tiling]]\nworkspaces = []\n{kitty}")).contains("`workspaces` is empty"));
        let bad_action = tiling_err(&format!("[[exclusive-tiling]]\nworkspaces = [\"T\"]\nelse-action = \"tile\"\n{kitty}"));
        assert!(bad_action.contains("expected `float`"), "{bad_action}");
        let no_app = tiling_err("[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [{ title = \"x\" }]\n");
        assert!(no_app.contains("missing field `app`"), "{no_app}");
        let bad_re = tiling_err("[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [{ app = \"a\", title = \"(\" }]\n");
        assert!(bad_re.contains("invalid title regex"), "{bad_re}");
        let negative = tiling_err("[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [{ app = \"a\", max-tiled = -1 }]\n");
        assert!(negative.contains("max-tiled"), "{negative}");
        let unknown = tiling_err("[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = [{ app = \"a\", max = 1 }]\n");
        assert!(unknown.contains("unknown field"), "{unknown}");
    }

    #[test]
    fn a_rule_without_an_allowlist_is_checked_top_level_and_per_profile() {
        let top = tiling_err("[[exclusive-tiling]]\nworkspaces = [\"T\"]\n");
        assert!(top.contains("workspace \"T\" has no tiling-allowlist (top-level)"), "{top}");
        // The profile supplies the allowlist, but top-level alone still lacks one.
        let partial = "[[exclusive-tiling]]\nworkspaces = [\"T\"]\n[[profile]]\nname = \"p\"\nwhen = {}\n\
            [[profile.exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = []\n";
        assert!(tiling_err(partial).contains("(top-level)"));
        let prof = "[[profile]]\nname = \"p\"\nwhen = {}\n[[profile.exclusive-tiling]]\nworkspaces = [\"T\"]\nelse-action = \"float\"\n";
        assert!(tiling_err(prof).contains("(profile \"p\")"));
        let empty_ok = parse("[[exclusive-tiling]]\nworkspaces = [\"T\"]\ntiling-allowlist = []\n").unwrap();
        assert!(empty_ok.config.has_exclusive_tiling(), "an empty allowlist is valid: nothing tiles");
    }

    #[test]
    fn the_commented_example_block_parses_when_uncommented() {
        let start = EXAMPLE.find("# [[exclusive-tiling]]").expect("example has the block");
        let block: String = EXAMPLE[start..].lines().map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l))).collect();
        let l = parse(&block).unwrap();
        let r = crate::tiling::effective(&l.config, None);
        assert_eq!((r.len(), r[0].workspace.as_str(), r[0].tiling_allowlist.len()), (1, "T", 2));
    }

    #[test]
    fn empty_file_is_a_valid_empty_config() {
        let l = parse("").unwrap();
        assert!(l.config.layout.is_empty() && l.config.placement.is_empty());
        assert_eq!(l.config.settings.debounce.trailing_ms, 1000);
        assert!(l.config.settings.warn_on_no_match);
    }
}
