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

impl Stats {
    /// Assemble a result from parts whose XP is already complete (base plus perfect bonuses).
    /// Keeping the level/rank arithmetic here means a stats aggregate cannot fork the rules.
    pub fn from_parts(xp: i64, perfect_days: Vec<String>, streaks: BTreeMap<String, i64>) -> Stats {
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
}

pub fn rank_for(level: i64) -> &'static str {
    RANKS[((level - 1).max(0) as usize).min(RANKS.len() - 1)]
}

pub(crate) fn parse(d: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()
}

/// Weekdays on which an item counts as scheduled, as a 7-bit mask (bit 0 = Monday).
///
/// An item only contributes to a day's schedule while it is live, active and a chore or habit.
/// The incremental stats ledger stores the same mask per weekday, so a schedule change is a
/// mask difference; the per-date creation bound is applied separately because it depends on the
/// date rather than the weekday. `scheduled_on` and the ledger therefore evaluate the same
/// predicate instead of two similar ones.
pub(crate) fn scheduled_weekday_bits(kind: &str, weekdays: i64, active: bool, deleted: bool) -> u8 {
    if deleted || !active || (kind != "chore" && kind != "habit") {
        return 0;
    }
    (weekdays as u8) & 0x7f
}

/// Whether an item exists yet on `date`. A missing creation date (every item that predates
/// TRACK-65, and any item an old client created before the server recorded one) has no lower
/// bound, so its history scores exactly as it did before the upgrade.
fn existed_on_parts(created_on: Option<&str>, date: NaiveDate) -> bool {
    created_on
        .and_then(parse)
        .is_none_or(|created| date > created)
}

/// A chore or habit is scheduled on `date` when it existed by then and its weekday bit is set.
///
/// The item counts only from the day AFTER creation, so adding one today cannot remove a
/// perfect-day bonus already earned today, and adding one later cannot rescore an earlier day.
fn scheduled_on(item: &Item, date: NaiveDate) -> bool {
    scheduled_on_parts(
        &item.kind,
        item.weekdays,
        item.active,
        item.deleted_at.is_some(),
        item.created_on.as_deref(),
        date,
    )
}

/// [`scheduled_on`] from already-extracted item fields, so the incremental ledger and the
/// full-history reference evaluate the same predicate instead of two similar ones.
pub(crate) fn scheduled_on_parts(
    kind: &str,
    weekdays: i64,
    active: bool,
    deleted: bool,
    created_on: Option<&str>,
    date: NaiveDate,
) -> bool {
    scheduled_weekday_bits(kind, weekdays, active, deleted)
        & (1 << date.weekday().num_days_from_monday())
        != 0
        && existed_on_parts(created_on, date)
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

    Stats::from_parts(xp, perfect_days, streaks)
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
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../../spec/xp_vectors.json")).unwrap();
        assert!(!cases.is_empty());
        for c in cases {
            let s = compute(&c.items, &c.completions, parse(&c.today).unwrap());
            assert_eq!(s.xp, c.expect.xp, "{}: xp", c.name);
            assert_eq!(s.level, c.expect.level, "{}: level", c.name);
            assert_eq!(
                s.perfect_days, c.expect.perfect_days,
                "{}: perfect days",
                c.name
            );
            assert_eq!(s.streaks, c.expect.streaks, "{}: streaks", c.name);
        }
    }

    #[test]
    fn ranks_clamp() {
        assert_eq!(rank_for(1), "Flour");
        assert_eq!(rank_for(7), "Sourdough Master");
        assert_eq!(rank_for(99), "Sourdough Master");
    }

    fn scheduled_item(id: &str, created_on: &str) -> Item {
        Item {
            id: id.to_string(),
            kind: "chore".to_string(),
            name: "Chore".to_string(),
            weekdays: 127,
            remind_time: None,
            points: 10,
            due_date: None,
            sort: 0,
            active: true,
            updated_at: 0,
            deleted_at: None,
            created_on: Some(created_on.to_string()),
            created_by: None,
            permission_override: None,
            seq: 0,
        }
    }

    fn habit_item(id: &str, created_on: &str) -> Item {
        Item {
            id: id.to_string(),
            kind: "habit".to_string(),
            name: "Habit".to_string(),
            weekdays: 127,
            remind_time: None,
            points: 15,
            due_date: None,
            sort: 0,
            active: true,
            updated_at: 0,
            deleted_at: None,
            created_on: Some(created_on.to_string()),
            created_by: None,
            permission_override: None,
            seq: 0,
        }
    }

    fn completion(item_id: &str, date: &str) -> Completion {
        Completion {
            item_id: item_id.to_string(),
            date: date.to_string(),
            completed_at: 0,
            updated_at: 0,
            deleted_at: None,
            seq: 0,
        }
    }

    #[test]
    fn adding_an_item_never_reduces_xp_or_perfect_days() {
        let today = parse("2026-10-20").unwrap();
        let mut items = vec![scheduled_item("a", "2026-10-01")];
        let completions = vec![
            completion("a", "2026-10-18"),
            completion("a", "2026-10-19"),
            completion("a", "2026-10-20"),
        ];
        let before = compute(&items, &completions, today);
        assert_eq!(before.perfect_days.len(), 3);

        // Created today or in the future: neither may remove a past day or today's bonus.
        for created_on in ["2026-10-20", "2026-10-27"] {
            items.push(scheduled_item("b", created_on));
            let after = compute(&items, &completions, today);
            assert!(
                after.xp >= before.xp,
                "adding an item created {created_on} reduced xp"
            );
            assert!(
                after.perfect_days.len() >= before.perfect_days.len(),
                "adding an item created {created_on} removed a perfect day"
            );
            items.pop();
        }
    }

    #[test]
    fn an_item_starts_counting_the_day_after_it_was_created() {
        let today = parse("2026-10-20").unwrap();
        let items = vec![scheduled_item("a", "2026-10-20")];
        let completions = vec![completion("a", "2026-10-20")];
        let stats = compute(&items, &completions, today);
        assert_eq!(stats.xp, 10, "the completion still earns its item XP");
        assert!(
            stats.perfect_days.is_empty(),
            "the creation day is not scheduled yet"
        );

        let completions = vec![completion("a", "2026-10-20"), completion("a", "2026-10-21")];
        let stats = compute(&items, &completions, parse("2026-10-21").unwrap());
        assert_eq!(stats.perfect_days, vec!["2026-10-21".to_string()]);
    }

    #[test]
    fn parent_assigned_items_score_exactly_like_owned_items() {
        let today = parse("2026-10-19").unwrap();
        let base = vec![
            scheduled_item("chore", "2026-10-01"),
            habit_item("habit", "2026-10-01"),
        ];
        let completions = vec![
            completion("chore", "2026-10-19"),
            completion("habit", "2026-10-17"),
            completion("habit", "2026-10-18"),
            completion("habit", "2026-10-19"),
        ];

        let owned = compute(&base, &completions, today);
        assert_eq!(owned.xp, 105, "chore, habit and perfect-day points");
        assert_eq!(owned.perfect_days, vec!["2026-10-19".to_string()]);
        assert_eq!(owned.streaks.get("habit"), Some(&3));

        // Provenance is display information only: an item the child owns (created_by = the
        // child), an item a parent assigned (created_by = another user) and a legacy row with
        // no provenance must produce byte-for-byte the same statistics.
        for created_by in [Some(1), Some(2), None] {
            let items: Vec<Item> = base
                .iter()
                .map(|item| Item {
                    created_by,
                    ..item.clone()
                })
                .collect();
            let stats = compute(&items, &completions, today);
            assert_eq!(
                stats, owned,
                "created_by = {created_by:?} changed the score"
            );
        }
    }

    #[test]
    fn a_parent_assigned_item_obeys_the_creation_date_bound() {
        let mut item = scheduled_item("parent", "2026-10-19");
        item.created_by = Some(2);

        let same_day = compute(
            std::slice::from_ref(&item),
            &[completion("parent", "2026-10-19")],
            parse("2026-10-19").unwrap(),
        );
        assert_eq!(same_day.xp, 10, "the completion still earns item XP");
        assert!(
            same_day.perfect_days.is_empty(),
            "the creation day is not scheduled yet"
        );

        let next_day = compute(
            &[item],
            &[completion("parent", "2026-10-20")],
            parse("2026-10-20").unwrap(),
        );
        assert_eq!(next_day.xp, 60, "10 points plus the perfect-day bonus");
        assert_eq!(next_day.perfect_days, vec!["2026-10-20".to_string()]);
    }
}
