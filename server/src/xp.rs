//! XP, level, perfect-day and streak rules. Pure functions, no I/O.
//!
//! The Dart app implements the same rules; `spec/xp_vectors.json` holds shared cases
//! that both test suites run so the two cannot drift. See DESIGN.md for the rule text.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{Datelike, Duration, NaiveDate};
use serde::Serialize;

use crate::model::{Completion, Item};

pub const PERFECT_DAY_BONUS: i64 = 50;
pub const XP_PER_LEVEL: i64 = 120;
pub const RANKS: [&str; 7] = [
    "Flour",
    "Dough",
    "Proofing",
    "Rising",
    "Baked",
    "Golden Crust",
    "Sourdough Master",
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stats {
    pub xp: i64,
    pub level: i64,
    pub rank: String,
    pub into_level: i64,
    pub per_level: i64,
    pub perfect_days: Vec<String>,
    pub streaks: BTreeMap<String, i64>,
}

pub fn rank_for(level: i64) -> &'static str {
    RANKS[((level - 1).max(0) as usize).min(RANKS.len() - 1)]
}

fn parse(d: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()
}

/// A chore or habit is scheduled on `date` when its weekday bit is set.
fn scheduled_on(item: &Item, date: NaiveDate) -> bool {
    item.active && (item.kind == "chore" || item.kind == "habit") && item.weekdays & (1 << date.weekday().num_days_from_monday()) != 0
}

pub fn compute(items: &[Item], completions: &[Completion], today: NaiveDate) -> Stats {
    let live: HashMap<&str, &Item> = items
        .iter()
        .filter(|i| i.deleted_at.is_none())
        .map(|i| (i.id.as_str(), i))
        .collect();

    // item id -> set of completed dates (un-ticked and orphaned rows excluded)
    let mut done: HashMap<&str, BTreeSet<NaiveDate>> = HashMap::new();
    let mut xp = 0i64;
    for c in completions.iter().filter(|c| c.deleted_at.is_none()) {
        let (Some(item), Some(date)) = (live.get(c.item_id.as_str()), parse(&c.date)) else {
            continue;
        };
        if done.entry(item.id.as_str()).or_default().insert(date) {
            xp += item.points;
        }
    }

    // Perfect day: at least one chore/habit scheduled, and every one of them done.
    let dates: BTreeSet<NaiveDate> = done.values().flatten().copied().collect();
    let mut perfect_days = Vec::new();
    for date in dates {
        let scheduled: Vec<&&Item> = live.values().filter(|i| scheduled_on(i, date)).collect();
        if !scheduled.is_empty()
            && scheduled
                .iter()
                .all(|i| done.get(i.id.as_str()).is_some_and(|s| s.contains(&date)))
        {
            xp += PERFECT_DAY_BONUS;
            perfect_days.push(date.format("%Y-%m-%d").to_string());
        }
    }

    // Streaks: consecutive completed days ending today, or yesterday if today is not done yet.
    let mut streaks = BTreeMap::new();
    for item in live.values().filter(|i| i.kind == "habit") {
        let set = done.get(item.id.as_str());
        let mut day = match set {
            Some(s) if s.contains(&today) => today,
            _ => today - Duration::days(1),
        };
        let mut n = 0;
        while set.is_some_and(|s| s.contains(&day)) {
            n += 1;
            day -= Duration::days(1);
        }
        streaks.insert(item.id.clone(), n);
    }

    let level = xp / XP_PER_LEVEL + 1;
    Stats {
        xp,
        level,
        rank: rank_for(level).to_string(),
        into_level: xp % XP_PER_LEVEL,
        per_level: XP_PER_LEVEL,
        perfect_days,
        streaks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Expect {
        xp: i64,
        level: i64,
        perfect_days: Vec<String>,
        streaks: BTreeMap<String, i64>,
    }
    #[derive(Deserialize)]
    struct Case {
        name: String,
        today: String,
        items: Vec<Item>,
        completions: Vec<Completion>,
        expect: Expect,
    }

    #[test]
    fn shared_vectors() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("../../spec/xp_vectors.json")).unwrap();
        assert!(!cases.is_empty());
        for c in cases {
            let s = compute(&c.items, &c.completions, parse(&c.today).unwrap());
            assert_eq!(s.xp, c.expect.xp, "{}: xp", c.name);
            assert_eq!(s.level, c.expect.level, "{}: level", c.name);
            assert_eq!(s.perfect_days, c.expect.perfect_days, "{}: perfect days", c.name);
            assert_eq!(s.streaks, c.expect.streaks, "{}: streaks", c.name);
        }
    }

    #[test]
    fn ranks_clamp() {
        assert_eq!(rank_for(1), "Flour");
        assert_eq!(rank_for(7), "Sourdough Master");
        assert_eq!(rank_for(99), "Sourdough Master");
    }
}
