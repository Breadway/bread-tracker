//! The `invite` subcommands, run against the built binary and a scratch database.

use std::process::{Command, Output};

fn run(db: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bread-tracker-server"))
        .args(args)
        .env("BREADTRACKER_DATABASE_PATH", db)
        .env_remove("BREADTRACKER_FCM_SERVICE_ACCOUNT")
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

#[test]
fn create_list_and_revoke() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");

    let o = run(&db, &["invite", "create"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let code = out(&o).trim().to_string();
    assert_eq!(code.len(), 14, "{code:?}");
    assert!(code.chars().all(|c| c == '-' || c.is_ascii_alphanumeric()));

    let o = run(&db, &["invite", "create", "--ttl", "48h"]);
    assert!(o.status.success());
    assert_ne!(out(&o).trim(), code, "codes are random");

    let list = out(&run(&db, &["invite", "list"]));
    assert_eq!(list.lines().count(), 2, "{list}");
    assert!(list.lines().all(|l| l.contains("active")));
    assert!(!list.contains(&code), "the list never shows codes");

    assert!(run(&db, &["invite", "revoke", "1"]).status.success());
    let list = out(&run(&db, &["invite", "list"]));
    assert!(list.lines().next().unwrap().contains("revoked"), "{list}");
    assert!(
        !run(&db, &["invite", "revoke", "1"]).status.success(),
        "already revoked"
    );
    assert!(
        !run(&db, &["invite", "revoke", "99"]).status.success(),
        "unknown id"
    );

    for bad in [
        &["invite", "create", "--ttl", "soon"][..],
        &["invite", "create", "--ttl"],
        &["invite"],
        &["invite", "revoke", "x"],
    ] {
        assert!(!run(&db, bad).status.success(), "{bad:?}");
    }
}
