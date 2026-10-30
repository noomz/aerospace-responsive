//! The top gap: what AeroSpace's `outer.top` should be for the attached monitors, and the
//! format-preserving edit that makes it so.
//!
//! Why a notch is compensated at all: AeroSpace lays out inside `visibleFrame` and adds the
//! gap on top of it (`tree/Monitor.swift:16`, `layout/layoutRecursive.swift:8` reach
//! `visibleRectPaddedByOuterGaps`), and a notched panel already has its notch height removed
//! from `visibleFrame`. A top gap sized to clear a bar therefore lands notch-height lower on a
//! notched display than on one whose `visibleFrame` starts at the physical top edge. Measuring
//! the notch (`display::notch_height`) is what makes the same configured number mean the same
//! distance from the usable edge everywhere.
//!
//! Writing the value: AeroSpace accepts either a plain int or an array of
//! `{ monitor.<pattern> = <int> }` entries ending in a plain default
//! (`config/DynamicConfigValue.swift:parseDynamicValue`). The patterns are matched in order
//! against the monitor resolved from the pattern, so this tool writes the 1-based sequence
//! number (`monitor.<id>`) — the same number as `aerospace`'s `%{monitor-id}` — because a
//! monitor *name* may be empty or positional ("LEN T23i-10 (1)"), while the id cannot be
//! ambiguous within one attached set.

use crate::rules::GapDecision;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use toml_edit::{DocumentMut, Item, Table};

/// One `outer.top` value in the shape AeroSpace accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopValue {
    /// `(monitor id, gap)` in id order — the exceptions; every other monitor gets `default`.
    pub patterns: Vec<(u32, u32)>,
    pub default: u32,
}

impl TopValue {
    /// The TOML expression. A bare int when every monitor agrees; otherwise the array form,
    /// which AeroSpace requires to hold at least one pattern and end in a plain int.
    pub fn to_toml(&self) -> String {
        if self.patterns.is_empty() {
            return self.default.to_string();
        }
        let parts: Vec<String> = self.patterns.iter().map(|(id, v)| format!("{{ monitor.{id} = {v} }}")).collect();
        format!("[{}, {}]", parts.join(", "), self.default)
    }
}

/// The value the config asks for, given this pass's per-monitor decisions. `None` = there is
/// no `[gaps]` block, so `outer.top` is not ours to touch.
pub fn desired(decisions: &[GapDecision]) -> Option<TopValue> {
    if decisions.is_empty() {
        return None;
    }
    // The trailing default is the value most monitors share, so the array lists exceptions.
    // A tie goes to the value held by the highest monitor id, so the line is deterministic.
    let mut counts: BTreeMap<u32, (usize, u32)> = BTreeMap::new();
    for d in decisions {
        let e = counts.entry(d.top).or_insert((0, 0));
        e.0 += 1;
        e.1 = e.1.max(d.monitor);
    }
    let default = *counts.iter().max_by_key(|(_, (n, id))| (*n, *id)).map(|(v, _)| v).expect("non-empty");
    Some(TopValue { patterns: decisions.iter().filter(|d| d.top != default).map(|d| (d.monitor, d.top)).collect(), default })
}

/// `gaps.outer.top` as written, `None` when it is absent or not in the shape this tool writes
/// (a name or `main` pattern, for instance — taking over from such a line is the point).
pub fn current(text: &str) -> Result<Option<TopValue>> {
    let doc: DocumentMut = text.parse().context("the AeroSpace config is not valid TOML")?;
    Ok(on_disk(&doc))
}

fn on_disk(doc: &DocumentMut) -> Option<TopValue> {
    as_top_value(doc.get("gaps")?.get("outer")?.get("top")?)
}

fn as_top_value(item: &Item) -> Option<TopValue> {
    let v = item.as_value()?;
    if let Some(n) = v.as_integer() {
        return (n >= 0).then_some(TopValue { patterns: vec![], default: n as u32 });
    }
    let arr = v.as_array()?;
    // AeroSpace's array form: 1-based patterns, then a plain-int default. One item is not it.
    if arr.len() < 2 {
        return None;
    }
    let default = arr.get(arr.len() - 1)?.as_integer()?;
    if default < 0 {
        return None;
    }
    let mut patterns = vec![];
    for e in arr.iter().take(arr.len() - 1) {
        let outer = e.as_inline_table()?;
        if outer.len() != 1 {
            return None;
        }
        let (key, m) = outer.iter().next()?;
        if key != "monitor" {
            return None;
        }
        let inner = m.as_inline_table()?;
        if inner.len() != 1 {
            return None;
        }
        let (pattern, gap) = inner.iter().next()?;
        let id: u32 = pattern.parse().ok()?;
        let n = gap.as_integer()?;
        if id < 1 || n < 0 {
            return None;
        }
        patterns.push((id, n as u32));
    }
    Some(TopValue { patterns, default: default as u32 })
}

/// The edit that makes `gaps.outer.top` read `want`. `changed = false` means the file already
/// says exactly that, so a steady state writes nothing and never reloads AeroSpace.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub want: TopValue,
    /// What the file says now (`None` = absent or a foreign shape).
    pub on_disk: Option<TopValue>,
    pub changed: bool,
    pub new_text: String,
}

pub fn plan(text: &str, want: &TopValue) -> Result<Plan> {
    let mut doc: DocumentMut = text.parse().context("the AeroSpace config is not valid TOML")?;
    let round_trips = doc.to_string() == text;
    let here = on_disk(&doc);
    if here.as_ref() == Some(want) {
        return Ok(Plan { want: want.clone(), on_disk: here, changed: false, new_text: text.to_string() });
    }
    if !round_trips {
        bail!(
            "the AeroSpace config does not round-trip through the format-preserving editor; \
             outer.top left alone. Set it by hand to: outer.top = {}",
            want.to_toml()
        );
    }
    // Render the expression in a scratch document first: a value we cannot express is our bug,
    // not the user's, and their file must stay untouched.
    let expr = want.to_toml();
    let mut probe: DocumentMut = format!("top = {expr}\n").parse().with_context(|| format!("internal: cannot render outer.top = {expr}"))?;
    let Item::Value(v) = probe.remove("top").expect("just written") else {
        bail!("internal: outer.top = {expr} did not render as a value");
    };
    let gaps = doc.entry("gaps").or_insert(Item::Table(Table::new()));
    let outer = gaps.as_table_mut().context("`gaps` in the AeroSpace config is not a table")?.entry("outer").or_insert(Item::Table(Table::new()));
    outer.as_table_mut().context("`gaps.outer` in the AeroSpace config is not a table")?.insert("top", Item::Value(v));
    let new_text = doc.to_string();
    // Read our own output back: this both proves the file is still valid TOML and that the
    // value landed where we think it did (a dotted `gaps.outer.top` in the original could
    // otherwise leave two definitions behind).
    match current(&new_text)? {
        Some(after) if &after == want => Ok(Plan { want: want.clone(), on_disk: here, changed: true, new_text }),
        other => bail!("the edit left outer.top as {other:?}, not {want:?}; nothing was written"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::GapDecision;

    fn d(monitor: u32, top: u32) -> GapDecision {
        GapDecision { monitor, monitor_name: format!("m{monitor}"), base: top, source: "x".into(), notch: 0.0, top }
    }

    #[test]
    fn one_number_for_every_monitor_stays_a_bare_int() {
        let w = desired(&[d(1, 20), d(2, 20)]).unwrap();
        assert_eq!(w.patterns, []);
        assert_eq!(w.to_toml(), "20");
        assert_eq!(desired(&[]), None, "no [gaps] block, no opinion");
    }

    #[test]
    fn exceptions_are_listed_and_the_common_value_is_the_default() {
        // Built-in (2) has its notch compensated; the two LENs keep the configured number.
        let w = desired(&[d(1, 52), d(2, 20), d(3, 52)]).unwrap();
        assert_eq!(w.default, 52);
        assert_eq!(w.patterns, [(2, 20)]);
        assert_eq!(w.to_toml(), "[{ monitor.2 = 20 }, 52]");
    }

    #[test]
    fn a_tie_for_the_default_breaks_by_highest_monitor_id() {
        let w = desired(&[d(1, 10), d(2, 20)]).unwrap();
        assert_eq!(w.default, 20, "one entry either way; the tie must resolve the same way every pass");
        assert_eq!(w.patterns, [(1, 10)]);
    }

    #[test]
    fn reads_both_shapes_aerospace_accepts() {
        assert_eq!(current("[gaps]\nouter.top = 5\n").unwrap().unwrap(), TopValue { patterns: vec![], default: 5 });
        assert_eq!(
            current("[gaps]\nouter.top = [\n  { monitor.2 = 20 },\n  { monitor.3 = 30 },\n  1,\n]\n").unwrap().unwrap(),
            TopValue { patterns: vec![(2, 20), (3, 30)], default: 1 }
        );
        assert_eq!(current("[gaps]\ninner.horizontal = 8\n").unwrap(), None, "no outer.top");
        assert_eq!(current("[gaps]\nouter.top = [{ monitor.main = 42 }, 1]\n").unwrap(), None, "a name pattern is not ours; take it over");
    }

    #[test]
    fn a_steady_state_writes_nothing() {
        let text = "[gaps]\nouter.top = [{ monitor.2 = 20 }, 52]\n";
        let want = desired(&[d(1, 52), d(2, 20), d(3, 52)]).unwrap();
        let p = plan(text, &want).unwrap();
        assert!(!p.changed);
        assert_eq!(p.new_text, text);
    }

    #[test]
    fn takes_over_a_foreign_line_and_keeps_the_rest_of_the_file() {
        let text = "# my aerospace config\nconfig-version = 2\n\n[gaps]\ninner.horizontal = 8   # tight\nouter.top = [{ monitor.main = 42 }, 1]\n";
        let want = desired(&[d(1, 52), d(2, 20), d(3, 52)]).unwrap();
        let p = plan(text, &want).unwrap();
        assert!(p.changed);
        assert_eq!(p.on_disk, None, "the old line was not ours");
        assert!(p.new_text.contains("outer.top = [{ monitor.2 = 20 }, 52]"), "{}", p.new_text);
        assert!(p.new_text.contains("inner.horizontal = 8   # tight"));
        assert!(p.new_text.contains("# my aerospace config"));
        // Idempotent: the second pass says nothing to do.
        assert!(!plan(&p.new_text, &want).unwrap().changed);
    }

    #[test]
    fn creates_the_tables_when_the_config_has_none() {
        let p = plan("config-version = 2\n", &TopValue { patterns: vec![], default: 42 }).unwrap();
        assert!(p.changed);
        assert_eq!(current(&p.new_text).unwrap().unwrap(), TopValue { patterns: vec![], default: 42 });
    }

    #[test]
    fn a_dotted_gaps_key_does_not_leave_two_definitions_behind() {
        let p = plan("gaps.outer.top = 5\n", &TopValue { patterns: vec![(2, 20)], default: 52 }).unwrap();
        assert_eq!(current(&p.new_text).unwrap().unwrap(), TopValue { patterns: vec![(2, 20)], default: 52 });
        assert_eq!(p.new_text.matches("top").count(), 1, "{}", p.new_text);
    }

    #[test]
    fn a_gaps_table_written_inline_is_refused_not_guessed() {
        // `gaps = { outer = { top = 1 } }` is valid TOML but not a table the editor can
        // append into; saying so beats rewriting the user's shape under them.
        let err = plan("gaps = { outer = { top = 1 } }\n", &TopValue { patterns: vec![], default: 9 }).unwrap_err();
        assert!(format!("{err:#}").contains("is not a table"), "{err:#}");
    }

    #[test]
    fn a_negative_or_foreign_value_is_not_ours() {
        assert_eq!(current("[gaps]\nouter.top = -3\n").unwrap(), None);
        assert_eq!(current("[gaps]\nouter.top = [\"x\", 1]\n").unwrap(), None);
        assert_eq!(current("[gaps]\nouter.top = [1]\n").unwrap(), None, "AeroSpace rejects an array with no pattern");
    }
}
