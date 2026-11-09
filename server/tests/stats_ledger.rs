//! TRACK-72: the incremental statistics ledger must return exactly what the shared full-history
//! rules return, for every retained history shape, while `/stats` stops walking completions.
//!
//! `xp::compute` is the reference implementation. Every test here drives real sync writes
//! through `UserDb::apply_push` and then compares `UserDb::stats` with that reference, so an
//! invalidation miss (item edit, deletion, tombstone, duplicate date encoding, multi-user
//! scoping) fails as a mismatch rather than as an approximation.

use std::borrow::Cow;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bread_tracker_server::model::{Completion, Item};
use bread_tracker_server::{db, store::UserDb, users, xp};
use chrono::NaiveDate;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

fn day(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn item(id: &str, kind: &str, weekdays: i64, points: i64) -> Item {
    Item {
        id: id.to_string(),
        kind: kind.to_string(),
        name: id.to_string(),
        weekdays,
        remind_time: None,
        points,
        due_date: None,
        sort: 0,
        active: true,
        updated_at: 1,
        deleted_at: None,
        // Most tests describe history that predates the item, so give the helper an old
        // creation date. A NULL (legacy) creation date is exercised in the populated upgrade
        // test, where rows are written directly and never pass through the sync path.
        created_on: Some("2020-01-01".to_string()),
        created_by: None,
        permission_override: None,
        seq: 0,
    }
}

fn created(base: &Item, created_on: &str) -> Item {
    Item {
        created_on: Some(created_on.to_string()),
        ..base.clone()
    }
}

fn edit(base: &Item, points: i64, weekdays: i64, active: bool, updated_at: i64) -> Item {
    Item {
        points,
        weekdays,
        active,
        updated_at,
        ..base.clone()
    }
}

fn comp(item_id: &str, date: &str, updated_at: i64) -> Completion {
    Completion {
        item_id: item_id.to_string(),
        date: date.to_string(),
        completed_at: updated_at,
        updated_at,
        deleted_at: None,
        seq: 0,
    }
}

fn untick(item_id: &str, date: &str, updated_at: i64) -> Completion {
    Completion {
        deleted_at: Some(updated_at),
        ..comp(item_id, date, updated_at)
    }
}

async fn reference(user: &UserDb, today: NaiveDate) -> xp::Stats {
    xp::compute(
        &user.all_items().await.unwrap(),
        &user.all_completions().await.unwrap(),
        today,
    )
}

async fn assert_parity(user: &UserDb, today: NaiveDate, step: &str) {
    assert_eq!(
        user.stats(today).await.unwrap(),
        reference(user, today).await,
        "{step}"
    );
}

#[tokio::test]
async fn ledger_matches_reference_through_edits_tombstones_and_date_encodings() {
    let db = db::connect_memory().await.unwrap();
    let user = UserDb::new(&db, 1);
    let today = day("2026-11-08");
    let a = item("a", "chore", 31, 10); // Mon-Fri
    let h = item("h", "habit", 127, 15);
    let t = item("t", "todo", 0, 20);
    let d = item("d", "chore", 127, 5);

    user.apply_push(
        &[a.clone(), h.clone(), t.clone(), d.clone()],
        &[
            comp("a", "2026-11-02", 10),
            comp("a", "2026-11-03", 10),
            comp("h", "2026-11-02", 10),
            comp("h", "2026-11-03", 10),
            comp("h", "2026-11-04", 10),
            comp("h", "2026-11-05", 10),
            comp("h", "2026-11-06", 10),
            comp("h", "2026-11-07", 10),
            comp("h", "2026-11-08", 10),
            comp("t", "2026-11-04", 10),
            comp("d", "2026-11-02", 10),
            comp("d", "2026-11-03", 10),
        ],
    )
    .await
    .unwrap();

    // Concrete expectations, not only self-consistency: both days have every scheduled
    // chore/habit done, so each adds 50; the rest is points per item-day.
    let stats = user.stats(today).await.unwrap();
    assert_eq!(
        stats.perfect_days,
        vec!["2026-11-02".to_string(), "2026-11-03".to_string()]
    );
    assert_eq!(stats.xp, 20 + 105 + 20 + 10 + 100);
    assert_parity(&user, today, "initial history").await;

    // Un-tick and re-tick the habit's today completion (streak and XP both move).
    user.apply_push(&[], &[untick("h", "2026-11-08", 20)])
        .await
        .unwrap();
    assert_parity(&user, today, "habit today un-ticked").await;
    user.apply_push(&[], &[comp("h", "2026-11-08", 30)])
        .await
        .unwrap();
    assert_parity(&user, today, "habit today re-ticked").await;

    // Historical edit: points change every completed day of that item.
    user.apply_push(&[edit(&a, 25, 31, true, 40)], &[])
        .await
        .unwrap();
    assert_parity(&user, today, "chore points edited").await;

    // Schedule edit: only Mondays now, which removes a perfect day.
    user.apply_push(&[edit(&a, 25, 1, true, 50)], &[])
        .await
        .unwrap();
    assert_parity(&user, today, "chore weekdays narrowed").await;

    // Deactivation removes it from schedules without removing earned XP.
    user.apply_push(&[edit(&a, 25, 1, false, 60)], &[])
        .await
        .unwrap();
    assert_parity(&user, today, "chore deactivated").await;

    // Deletion removes its XP and schedule membership; resurrection restores both.
    let mut deleted = edit(&a, 25, 1, false, 70);
    deleted.deleted_at = Some(70);
    user.apply_push(&[deleted], &[]).await.unwrap();
    assert_parity(&user, today, "chore deleted").await;

    // An old offline client may still tick a deleted item; it earns nothing until an item with
    // that id is live again.
    user.apply_push(&[], &[comp("a", "2026-11-05", 80)])
        .await
        .unwrap();
    assert_parity(&user, today, "completion for deleted item").await;

    let mut resurrected = edit(&a, 25, 1, true, 90);
    resurrected.deleted_at = None;
    user.apply_push(&[resurrected], &[]).await.unwrap();
    assert_parity(&user, today, "chore resurrected").await;

    // Kind changes move an item into and out of the streak map.
    user.apply_push(&[edit(&h, 15, 127, true, 100)], &[])
        .await
        .unwrap();
    assert_parity(&user, today, "habit kind fields unchanged").await;
    let mut h_as_chore = h.clone();
    h_as_chore.kind = "chore".to_string();
    h_as_chore.updated_at = 110;
    user.apply_push(&[h_as_chore], &[]).await.unwrap();
    assert_parity(&user, today, "habit became chore").await;
    let mut h_again = h.clone();
    h_again.updated_at = 120;
    user.apply_push(&[h_again], &[]).await.unwrap();
    assert_parity(&user, today, "chore became habit again").await;

    // Deleting a scheduled chore removes its perfect-day contribution.
    let mut d_deleted = d.clone();
    d_deleted.deleted_at = Some(130);
    d_deleted.updated_at = 130;
    user.apply_push(&[d_deleted], &[]).await.unwrap();
    assert_parity(&user, today, "scheduled chore deleted").await;

    // To-dos never schedule, but their points still change.
    user.apply_push(&[edit(&t, 30, 127, true, 140)], &[])
        .await
        .unwrap();
    assert_parity(&user, today, "todo points edited").await;

    // Two stored encodings of one calendar date count once, exactly as the set in the rules.
    user.apply_push(&[], &[comp("h", "2026-11-4", 150)])
        .await
        .unwrap();
    assert_parity(&user, today, "duplicate date encoding added").await;
    let before = user.stats(today).await.unwrap().xp;
    user.apply_push(&[], &[untick("h", "2026-11-04", 160)])
        .await
        .unwrap();
    assert_parity(&user, today, "one encoding of a duplicated date un-ticked").await;
    assert_eq!(
        user.stats(today).await.unwrap().xp,
        before,
        "the remaining encoding keeps the day"
    );
    user.apply_push(&[], &[untick("h", "2026-11-4", 170)])
        .await
        .unwrap();
    assert_parity(&user, today, "last encoding of the date un-ticked").await;
}

/// TRACK-65 reconciliation: the schedule ledger must apply each item's immutable creation date
/// per date. An item created on a day does not count toward that day's schedule, so adding one
/// cannot remove a perfect day that was already earned; it does count from the next day.
///
/// This would fail if the created_on bound were ignored: the new item would join the weekday
/// total for 2026-11-02 while only the old item is done there, and the already-earned perfect
/// day would disappear.
#[tokio::test]
async fn creation_dates_bound_the_schedule_ledger_per_date() {
    let db = db::connect_memory().await.unwrap();
    let user = UserDb::new(&db, 1);
    let today = day("2026-11-08");
    // Monday to Sunday, created before the history it scores.
    let a = created(&item("a", "chore", 127, 10), "2026-10-01");

    user.apply_push(
        std::slice::from_ref(&a),
        &[
            comp("a", "2026-11-02", 10),
            comp("a", "2026-11-03", 10),
            comp("a", "2026-11-04", 10),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        user.stats(today).await.unwrap().perfect_days,
        vec![
            "2026-11-02".to_string(),
            "2026-11-03".to_string(),
            "2026-11-04".to_string()
        ]
    );

    // Created ON 2026-11-02: it is not scheduled that day, so the earned day stays perfect.
    // From 2026-11-03 it is scheduled and not done, so those days stop being perfect.
    let b = created(&item("b", "chore", 127, 20), "2026-11-02");
    user.apply_push(std::slice::from_ref(&b), &[])
        .await
        .unwrap();
    let stats = user.stats(today).await.unwrap();
    assert_eq!(
        stats.perfect_days,
        vec!["2026-11-02".to_string()],
        "adding an item created on 2026-11-02 must not remove that day's bonus"
    );
    assert_parity(&user, today, "item created on the perfect day").await;

    // Completing it on its creation day earns item XP but does not make that day scheduled.
    user.apply_push(&[], &[comp("b", "2026-11-02", 20)])
        .await
        .unwrap();
    assert_parity(&user, today, "completion on the creation day").await;
    user.apply_push(
        &[],
        &[comp("b", "2026-11-03", 30), comp("b", "2026-11-04", 30)],
    )
    .await
    .unwrap();
    assert_parity(&user, today, "created item completed from the next day").await;
    assert_eq!(
        user.stats(today).await.unwrap().perfect_days,
        vec![
            "2026-11-02".to_string(),
            "2026-11-03".to_string(),
            "2026-11-04".to_string()
        ]
    );

    // An item synced without a creation date gets the first-seen date, so it does not reach
    // back into history either. (A stored legacy NULL is covered by the populated upgrade
    // test, where rows are written directly.)
    let future = created(&item("c", "chore", 1, 5), "2026-11-09"); // Mondays only
    user.apply_push(std::slice::from_ref(&future), &[])
        .await
        .unwrap();
    assert_eq!(
        user.stats(today).await.unwrap().perfect_days,
        vec![
            "2026-11-02".to_string(),
            "2026-11-03".to_string(),
            "2026-11-04".to_string()
        ],
        "an item created after the history must not change its schedule"
    );
    assert_parity(&user, today, "item created after the history").await;

    // A stored creation date never changes: an edit that sends another one is ignored.
    let mut moved = created(&b, "2026-11-04");
    moved.updated_at = 40;
    user.apply_push(&[moved], &[]).await.unwrap();
    let stored = user
        .all_items()
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.id == "b")
        .unwrap();
    assert_eq!(stored.created_on.as_deref(), Some("2026-11-02"));
    assert_parity(&user, today, "edit trying to move created_on").await;

    // Deactivation and resurrection move the bound with the item's schedule membership.
    let mut inactive = created(&b, "2026-11-02");
    inactive.active = false;
    inactive.updated_at = 50;
    user.apply_push(&[inactive], &[]).await.unwrap();
    assert_parity(&user, today, "created item deactivated").await;
    let mut active = created(&b, "2026-11-02");
    active.updated_at = 60;
    user.apply_push(&[active], &[]).await.unwrap();
    assert_parity(&user, today, "created item reactivated").await;

    // Deletion removes both its XP and its creation bound; resurrection restores both.
    let mut deleted = created(&b, "2026-11-02");
    deleted.deleted_at = Some(70);
    deleted.updated_at = 70;
    user.apply_push(&[deleted], &[]).await.unwrap();
    assert_parity(&user, today, "created item deleted").await;
    let mut resurrected = created(&b, "2026-11-02");
    resurrected.updated_at = 80;
    user.apply_push(&[resurrected], &[]).await.unwrap();
    assert_parity(&user, today, "created item resurrected").await;

    // Weekday edits change membership only from the creation date onward.
    let mut narrowed = created(&b, "2026-11-02");
    narrowed.weekdays = 1; // Mondays
    narrowed.updated_at = 90;
    user.apply_push(&[narrowed], &[]).await.unwrap();
    assert_parity(&user, today, "created item weekdays narrowed").await;
}

#[tokio::test]
async fn ledgers_are_scoped_to_one_account() {
    let db = db::connect_memory().await.unwrap();
    let other = users::create(&db, "Pat", false).await.unwrap();
    let alice = UserDb::new(&db, 1);
    let pat = UserDb::new(&db, other);
    let today = day("2026-11-08");
    let shared = item("s", "habit", 127, 10);

    alice
        .apply_push(
            std::slice::from_ref(&shared),
            &[comp("s", "2026-11-07", 10), comp("s", "2026-11-08", 10)],
        )
        .await
        .unwrap();
    pat.apply_push(
        std::slice::from_ref(&shared),
        &[comp("s", "2026-11-08", 10)],
    )
    .await
    .unwrap();
    assert_parity(&alice, today, "alice").await;
    assert_parity(&pat, today, "pat").await;

    let alice_stats = alice.stats(today).await.unwrap();
    let pat_stats = pat.stats(today).await.unwrap();
    assert_eq!(alice_stats.streaks.get("s"), Some(&2));
    assert_eq!(pat_stats.streaks.get("s"), Some(&1));
    assert_ne!(alice_stats.xp, pat_stats.xp);

    // Pat's later edit must not touch Alice's ledger.
    let mut pat_shared = shared.clone();
    pat_shared.points = 99;
    pat_shared.updated_at = 20;
    pat.apply_push(&[pat_shared], &[]).await.unwrap();
    assert_eq!(
        alice.stats(today).await.unwrap(),
        alice_stats,
        "another account's edit changed Alice's stats"
    );
    assert_parity(&pat, today, "pat after points edit").await;
}

async fn file_db(path: &Path) -> SqlitePool {
    db::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap()
}

#[tokio::test]
async fn concurrent_sync_and_stats_never_expose_a_partial_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let pool = file_db(&dir.path().join("concurrent.db")).await;
    let user = UserDb::new(&pool, 1);
    let today = day("2026-11-08");
    let base = item("a", "chore", 127, 10);
    user.apply_push(std::slice::from_ref(&base), &[comp("a", "2026-10-01", 1)])
        .await
        .unwrap();

    for round in 0..20 {
        let pre = user.stats(today).await.unwrap();
        let date = format!("2026-10-{:02}", round + 2);
        let updated = 10 + round as i64;
        user.apply_push(&[], &[comp("a", &date, updated)])
            .await
            .unwrap();
        let post = user.stats(today).await.unwrap();
        assert_ne!(pre.xp, post.xp, "round {round} must change XP");

        // A reader that runs while the next write commits must see a whole ledger: either the
        // state before that write or after it, never a mix of base XP, days and streaks.
        let stop = Arc::new(AtomicBool::new(false));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let user = user.clone();
            let stop = stop.clone();
            let pre = pre.clone();
            let post = post.clone();
            readers.push(tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let seen = user.stats(today).await.unwrap();
                    assert!(
                        seen == pre || seen == post,
                        "torn stats read: {seen:?} was neither {pre:?} nor {post:?}"
                    );
                }
            }));
        }
        let next_date = format!("2026-10-{:02}", round + 2);
        user.apply_push(
            &[],
            &[Completion {
                item_id: "a".to_string(),
                date: next_date,
                completed_at: 100,
                updated_at: 100 + round as i64,
                deleted_at: Some(100 + round as i64),
                seq: 0,
            }],
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        stop.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.await.unwrap();
        }
        assert_parity(&user, today, "after concurrent round").await;
    }
}

/// The migration set a deployment has before the statistics ledger existed.
fn pre_ledger_migrator() -> Migrator {
    // Everything before 0014, including the reserved 0004 limiter and 0013's created_on.
    let versions = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
    let mut m = sqlx::migrate!("../migrations");
    m.migrations = Cow::Owned(
        m.migrations
            .iter()
            .filter(|x| versions.contains(&x.version))
            .cloned()
            .collect(),
    );
    m
}

#[tokio::test]
async fn populated_legacy_upgrade_builds_the_ledger_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    pre_ledger_migrator().run(&pool).await.unwrap();

    // Real populated history, including a second account, an un-ticked row, a deleted item, a
    // padded date, an unpadded accepted date, a duplicate encoding of one date, and a date the
    // shared parser rejects (which must never count).
    let pat: i64 = sqlx::query_scalar(
        "INSERT INTO users (name, is_admin, created_at) VALUES ('Pat', 0, 0) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(&format!(
        "INSERT INTO items (user_id, id, kind, name, weekdays, points, active, updated_at, deleted_at, seq) VALUES
           (1, 'a', 'chore', 'Dishes', 127, 10, 1, 1, NULL, 1),
           (1, 'h', 'habit', 'Read', 127, 15, 1, 1, NULL, 2),
           (1, 't', 'todo', 'Rent', 0, 20, 1, 1, NULL, 3),
           (1, 'd', 'chore', 'Gone', 127, 5, 1, 1, 9, 4),
           (1, 'u', 'chore', 'Unused', 127, 7, 0, 1, NULL, 5);
         INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, deleted_at, seq) VALUES
           (1, 'a', '2026-11-01', 1, 1, NULL, 6),
           (1, 'a', '2026-11-02', 1, 1, NULL, 7),
           (1, 'h', '2026-11-01', 1, 1, NULL, 8),
           (1, 'h', '2026-11-02', 1, 1, NULL, 9),
           (1, 'h', '2026-11-3', 1, 1, NULL, 10),
           (1, 'h', '2026-11-03', 1, 1, NULL, 11),
           (1, 'h', '2026-11-04', 1, 1, 99, 12),
           (1, 't', '2026-11-02', 1, 1, NULL, 13),
           (1, 'd', '2026-11-02', 1, 1, NULL, 14),
           (1, 'h', '2026-02-30', 1, 1, NULL, 15);
         INSERT INTO items (user_id, id, kind, name, weekdays, points, active, updated_at, deleted_at, created_on, seq) VALUES
           (1, 'e', 'chore', 'Bounded', 127, 7, 1, 1, NULL, '2026-11-01', 16);
         INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, deleted_at, seq) VALUES
           (1, 'e', '2026-11-01', 1, 1, NULL, 17),
           (1, 'e', '2026-11-02', 1, 1, NULL, 18);
         INSERT INTO items (user_id, id, kind, name, weekdays, points, active, updated_at, deleted_at, seq) VALUES
           ({pat}, 'a', 'habit', 'Pat habit', 127, 30, 1, 1, NULL, 19);
         INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, deleted_at, seq) VALUES
           ({pat}, 'a', '2026-11-06', 1, 1, NULL, 20),
           ({pat}, 'a', '2026-11-07', 1, 1, NULL, 21);"
    ))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    drop(pool);

    // A normal start: migration 0014 applies and the one-time backfill builds every account.
    let pool = db::connect(&url).await.unwrap();
    let today = day("2026-11-08");
    for (uid, label) in [(1_i64, "admin"), (pat, "pat")] {
        let user = UserDb::new(&pool, uid);
        assert_parity(&user, today, label).await;
    }
    let built: Vec<(i64, i64)> =
        sqlx::query_as("SELECT user_id, built FROM stats_state ORDER BY user_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(built, vec![(1, 1), (pat, 1)]);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM completions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 14, "the upgrade must not delete history");

    // The un-ticked row contributes nothing, and the unparseable legacy date is ignored by the
    // snapshot exactly as the reference ignores it.
    let stats = UserDb::new(&pool, 1).stats(today).await.unwrap();
    assert_eq!(
        UserDb::new(&pool, 1).all_completions().await.unwrap().len(),
        12,
        "user 1 keeps all stored rows, including its tombstone"
    );
    assert!(stats.streaks.contains_key("h"));
    // The legacy NULL rows have no lower bound, so they count from the beginning of the
    // retained history: 11-01 was always scheduled for a and h, and 11-02 adds the item
    // created on 11-01 (which first counts the day after). The dead and inactive items never
    // count, and the unparseable legacy date is ignored.
    assert_eq!(
        stats.perfect_days,
        vec!["2026-11-01".to_string(), "2026-11-02".to_string()],
        "the backfilled ledger must keep the same schedule under created_on"
    );
    let again = db::connect(&url).await.unwrap();
    assert_eq!(
        UserDb::new(&again, 1).stats(today).await.unwrap(),
        stats,
        "a second start must not rebuild or change the ledger"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM stats_state WHERE built = 0")
            .fetch_one(&again)
            .await
            .unwrap(),
        0
    );
}

/// Populate a realistic large history directly (bypassing sync for speed) and rebuild the
/// ledger with the production rebuild path, then measure the actual `/stats` statements.
#[tokio::test]
async fn stats_work_does_not_scale_with_completion_count() {
    let pool = db::connect_memory().await.unwrap();
    sqlx::raw_sql(
        "WITH RECURSIVE n(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM n WHERE x < 9999)
         INSERT INTO items (user_id, id, kind, name, weekdays, points, active, updated_at, created_on, seq)
         SELECT 1, printf('i%d', x),
                CASE WHEN x < 10 THEN 'habit' ELSE 'todo' END,
                'item', CASE WHEN x < 10 THEN 127 ELSE 0 END, 10, 1, 1,
                CASE WHEN x < 10 THEN '2026-01-01' ELSE NULL END, x
         FROM n;
         WITH RECURSIVE n(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM n WHERE x < 1999)
         INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, seq)
         SELECT 1, printf('i%d', x % 10),
                date('2026-01-01', '+' || (x / 10) || ' days'), 1, 1, x
         FROM n;
         WITH RECURSIVE n(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM n WHERE x < 117999)
         INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, seq)
         SELECT 1, printf('i%d', 10 + ((x * 7) % 9990)),
                date('2024-01-01', '+' || (x % 1400) || ' days'), 1, 1, 2000 + x
         FROM n;
         ANALYZE;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let user = UserDb::new(&pool, 1);
    user.rebuild_stats().await.unwrap();
    let today = day("2026-11-08");

    let items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE user_id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let completions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM completions WHERE user_id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((items, completions), (10_000, 120_000));

    // Correctness at scale, against the same full-history reference used everywhere else.
    assert_parity(&user, today, "large synthetic history").await;

    // The read never touches completions and uses indexes rather than table scans of items.
    let plans = user.stats_query_plans().await.unwrap();
    for plan in &plans {
        assert!(
            !plan.contains("completions"),
            "/stats plan referenced completions: {plans:?}"
        );
        assert!(
            !plan.contains("SCAN items"),
            "/stats plan scanned items: {plans:?}"
        );
    }

    // Work of the production statements, in SQLite VM steps.
    let mut conn = pool.acquire().await.unwrap();
    let steps = Arc::new(AtomicUsize::new(0));
    let counter = steps.clone();
    conn.lock_handle()
        .await
        .unwrap()
        .set_progress_handler(1, move || {
            counter.fetch_add(1, Ordering::Relaxed);
            true
        });

    steps.store(0, Ordering::Relaxed);
    let ledger_stats = user.stats_on(&mut conn, today).await.unwrap();
    let ledger_steps = steps.load(Ordering::Relaxed);

    // The old shape: load every retained row, then compute in Rust. Count the same way for a
    // fair statement-level comparison (the Rust pass over those rows is additional work).
    steps.store(0, Ordering::Relaxed);
    let all_items: Vec<(String,)> = sqlx::query_as("SELECT id FROM items WHERE user_id = ?")
        .bind(1_i64)
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    let all_completions: Vec<(String, String)> =
        sqlx::query_as("SELECT item_id, date FROM completions WHERE user_id = ?")
            .bind(1_i64)
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    let full_history_steps = steps.load(Ordering::Relaxed);

    println!(
        "stats: {} VM steps, {} rows out; full-history load: {} VM steps, {} rows",
        ledger_steps,
        ledger_stats.perfect_days.len() + ledger_stats.streaks.len(),
        full_history_steps,
        all_items.len() + all_completions.len()
    );
    assert_eq!(all_completions.len(), 120_000);
    assert!(
        ledger_steps < 50_000,
        "ledger stats took {ledger_steps} steps for 120k completions"
    );
    assert!(
        ledger_steps * 20 < full_history_steps,
        "ledger stats ({ledger_steps}) should be far below the full-history load ({full_history_steps})"
    );
}
