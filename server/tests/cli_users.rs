//! The user, pair and devices subcommands, run against the built binary and a scratch database.

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

/// Run SQL against the scratch database with sqlx (no dependency on a sqlite3 binary).
fn exec(db: &std::path::Path, sql: &str) {
    use sqlx::Executor;
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display())).await.unwrap();
        pool.execute(sql).await.unwrap();
        pool.close().await;
    });
}

fn query(db: &std::path::Path, sql: &str) -> String {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display())).await.unwrap();
        // Every query here selects one column; COUNT(*) comes back as an integer, names as text.
        let row = sqlx::query(sql).fetch_one(&pool).await.unwrap();
        let v = match sqlx::Row::try_get::<i64, _>(&row, 0) {
            Ok(n) => n.to_string(),
            Err(_) => sqlx::Row::get::<String, _>(&row, 0),
        };
        pool.close().await;
        v
    })
}

/// Create a user directly in the file, the way signup would, and give them a device and an item.
fn add_user(db: &std::path::Path, name: &str) {
    exec(
        db,
        &format!(
            "INSERT INTO users (name, is_admin, created_at) VALUES ('{name}', 0, 1);
             INSERT INTO devices (id, user_id, name, token_hash, created_at, last_seen)
               VALUES ('dev-{name}', (SELECT id FROM users WHERE name = '{name}'), '{name} phone', 'hash-{name}', 1, 1);
             INSERT INTO items (user_id, id, kind, name, points, updated_at, seq)
               VALUES ((SELECT id FROM users WHERE name = '{name}'), 'item-{name}', 'chore', 'x', 10, 1, 1);"
        ),
    );
}

#[test]
fn pair_targets_the_admin_by_default_and_a_named_user_with_user() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    assert!(run(&db, &["user", "list"]).status.success()); // creates and migrates
    add_user(&db, "Bob");

    let o = run(&db, &["pair"]);
    assert!(o.status.success());
    assert_eq!(out(&o).trim().len(), 9, "XXXX-XXXX");
    assert!(err(&o).contains("admin"), "{}", err(&o));
    assert_eq!(query(&db, "SELECT COUNT(*) FROM pairing_codes WHERE user_id = 1"), "1");

    let o = run(&db, &["pair", "--user", "bob"]); // case-insensitive
    assert!(o.status.success(), "{}", err(&o));
    assert!(err(&o).contains("Bob"));
    assert_eq!(query(&db, "SELECT COUNT(*) FROM pairing_codes WHERE user_id = (SELECT id FROM users WHERE name = 'Bob')"), "1");
    assert_eq!(query(&db, "SELECT COUNT(*) FROM pairing_codes WHERE user_id = 1"), "1", "the admin's code was not touched");

    // An unknown user is an error, never a fallback to the admin.
    let before = query(&db, "SELECT COUNT(*) FROM pairing_codes");
    let o = run(&db, &["pair", "--user", "nobody"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("no user named"), "{}", err(&o));
    assert_eq!(query(&db, "SELECT COUNT(*) FROM pairing_codes"), before);
    assert!(!run(&db, &["pair", "--user"]).status.success(), "--user needs a value");
}

#[test]
fn devices_lists_all_or_one_users_and_names_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    run(&db, &["user", "list"]);
    add_user(&db, "Bob");
    add_user(&db, "Cy");
    let all = out(&run(&db, &["devices"]));
    assert_eq!(all.lines().count(), 2, "{all}");
    assert!(all.contains("Bob") && all.contains("Cy"));
    let bob = out(&run(&db, &["devices", "--user", "Bob"]));
    assert_eq!(bob.lines().count(), 1, "{bob}");
    assert!(bob.contains("dev-Bob") && !bob.contains("Cy"));
    assert!(!run(&db, &["devices", "--user", "ghost"]).status.success());
}

#[test]
fn user_list_and_remove_need_confirmation_and_protect_the_admin() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    run(&db, &["user", "list"]);
    add_user(&db, "Bob");
    add_user(&db, "Cy");

    let list = out(&run(&db, &["user", "list"]));
    assert_eq!(list.lines().count(), 3, "{list}");
    assert!(list.lines().next().unwrap().contains("admin"));
    assert!(list.contains("Bob") && list.contains("Cy"));

    // Without --yes nothing is deleted.
    let o = run(&db, &["user", "remove", "Bob"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    assert_eq!(query(&db, "SELECT COUNT(*) FROM users"), "3");

    // The admin cannot be removed, even with --yes.
    let o = run(&db, &["user", "remove", "admin", "--yes"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("admin"), "{}", err(&o));
    assert_eq!(query(&db, "SELECT COUNT(*) FROM users"), "3");

    // Unknown user.
    assert!(!run(&db, &["user", "remove", "ghost", "--yes"]).status.success());

    // With --yes, Bob and his rows are gone, Cy is untouched.
    let o = run(&db, &["user", "remove", "bob", "--yes"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("1 device(s), 1 item(s)"), "{}", out(&o));
    assert_eq!(query(&db, "SELECT COUNT(*) FROM users"), "2");
    assert_eq!(query(&db, "SELECT COUNT(*) FROM devices"), "1");
    assert_eq!(query(&db, "SELECT COUNT(*) FROM items"), "1");
    assert_eq!(query(&db, "SELECT COUNT(*) FROM user_settings"), "2");
    assert_eq!(query(&db, "SELECT name FROM users WHERE id > 1"), "Cy");
}

#[test]
fn bad_usage_fails_without_changing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    run(&db, &["user", "list"]);
    for bad in [&["user"][..], &["user", "frobnicate"], &["user", "remove"], &["user", "remove", "--yes"]] {
        assert!(!run(&db, bad).status.success(), "{bad:?}");
    }
    assert_eq!(query(&db, "SELECT COUNT(*) FROM users"), "1");
}

#[test]
fn disable_and_enable_keep_all_data_and_protect_the_admin() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    run(&db, &["user", "list"]);
    add_user(&db, "Bob");

    let o = run(&db, &["user", "disable", "bob"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(query(&db, "SELECT COUNT(*) FROM users WHERE disabled_at IS NOT NULL"), "1");
    assert_eq!(query(&db, "SELECT COUNT(*) FROM devices"), "1", "data is untouched");
    assert_eq!(query(&db, "SELECT COUNT(*) FROM items"), "1");
    let list = out(&run(&db, &["user", "list"]));
    assert!(list.lines().any(|l| l.contains("Bob") && l.contains("DISABLED")), "{list}");

    let o = run(&db, &["user", "enable", "Bob"]);
    assert!(o.status.success());
    assert_eq!(query(&db, "SELECT COUNT(*) FROM users WHERE disabled_at IS NOT NULL"), "0");
    assert!(!out(&run(&db, &["user", "list"])).contains("DISABLED"));

    let o = run(&db, &["user", "disable", "admin"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("admin"), "{}", err(&o));
    assert!(!run(&db, &["user", "disable", "ghost"]).status.success());
    assert!(!run(&db, &["user", "disable"]).status.success());
}
