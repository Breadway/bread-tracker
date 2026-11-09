//! `family list` and `family revoke`, run against the built binary and a scratch database.

use std::process::{Command, Output};

fn run(db: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bread-tracker-server"))
        .args(args)
        .env("BREADTRACKER_DATABASE_PATH", db)
        .env_remove("BREADTRACKER_FCM_SERVICE_ACCOUNT")
        .env_remove("BREADTRACKER_ADMIN_NAME")
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

fn exec(db: &std::path::Path, sql: &str) {
    use sqlx::Executor;
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display()))
            .await
            .unwrap();
        pool.execute(sql).await.unwrap();
        pool.close().await;
    });
}

fn query(db: &std::path::Path, sql: &str) -> String {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display()))
            .await
            .unwrap();
        let row = sqlx::query(sql).fetch_one(&pool).await.unwrap();
        let value = match sqlx::Row::try_get::<i64, _>(&row, 0) {
            Ok(n) => n.to_string(),
            Err(_) => sqlx::Row::get::<String, _>(&row, 0),
        };
        pool.close().await;
        value
    })
}

fn seed(db: &std::path::Path) {
    exec(
        db,
        "INSERT INTO users (name, is_admin, created_at)
           VALUES ('Parent', 0, 1), ('Kid', 0, 1), ('Other Parent', 0, 1);
         INSERT INTO family_links
           (parent_id, child_id, status, default_permission, notify_on_complete, created_at, accepted_at, pending_expires_at)
           VALUES
             ((SELECT id FROM users WHERE name = 'Parent'), (SELECT id FROM users WHERE name = 'Kid'),
              'active', 'full', 0, 1000, 1000, NULL),
             ((SELECT id FROM users WHERE name = 'Other Parent'), (SELECT id FROM users WHERE name = 'Kid'),
              'pending', 'complete_only', 0, 2000, NULL, 3000);",
    );
}

#[test]
fn family_list_shows_both_sides_and_revoke_targets_exactly_one_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    assert!(run(&db, &["user", "list"]).status.success()); // creates and migrates
    seed(&db);

    let o = run(&db, &["family", "list"]);
    assert!(o.status.success(), "{}", err(&o));
    let text = out(&o);
    assert!(text.contains("Parent -> Kid"), "{text}");
    assert!(text.contains("active"), "{text}");
    assert!(text.contains("full"), "{text}");
    assert!(text.contains("Other Parent -> Kid"), "{text}");
    assert!(text.contains("pending"), "{text}");
    assert!(text.contains("complete_only"), "{text}");

    // Every malformed form is a usage error and changes nothing.
    for args in [
        vec!["family"],
        vec!["family", "list", "extra"],
        vec!["family", "revoke"],
        vec!["family", "revoke", "abc"],
        vec!["family", "revoke", "1", "2"],
        vec!["family", "revoke", "--yes"],
        vec!["family", "revoke", "-1"],
    ] {
        let o = run(&db, &args);
        assert!(!o.status.success(), "{args:?} should fail");
        assert!(
            err(&o).contains("usage"),
            "{args:?} should print usage: {}",
            err(&o)
        );
        assert_eq!(
            query(&db, "SELECT status FROM family_links WHERE id = 1"),
            "active",
            "{args:?} must not revoke anything"
        );
    }

    let o = run(&db, &["family", "revoke", "1"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("Revoked family link 1"), "{}", out(&o));
    assert!(out(&o).contains("Parent -> Kid"), "{}", out(&o));
    assert_eq!(
        query(&db, "SELECT status FROM family_links WHERE id = 1"),
        "revoked"
    );
    assert_eq!(
        query(&db, "SELECT status FROM family_links WHERE id = 2"),
        "pending",
        "the other link was not touched"
    );
    assert_eq!(
        query(
            &db,
            "SELECT COUNT(*) FROM family_links WHERE id = 1 AND revoked_at IS NOT NULL"
        ),
        "1",
        "revocation records when it happened"
    );

    // A second revoke is refused, and an unknown id never falls back to another link.
    let o = run(&db, &["family", "revoke", "1"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("already revoked"), "{}", err(&o));
    let o = run(&db, &["family", "revoke", "999"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("no family link 999"), "{}", err(&o));
    assert_eq!(
        query(&db, "SELECT status FROM family_links WHERE id = 2"),
        "pending"
    );
}

#[test]
fn family_list_is_empty_before_any_link_exists() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    assert!(run(&db, &["user", "list"]).status.success());
    let o = run(&db, &["family", "list"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o).trim(), "No family links.");
}
