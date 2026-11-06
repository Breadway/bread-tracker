//! SQLite pool setup. WAL + foreign keys on, migrations embedded from `migrations/`.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

pub type Db = SqlitePool;

pub async fn connect(database_url: &str) -> anyhow::Result<Db> {
    connect_with_admin(database_url, None).await
}

/// Like [`connect`], and names the first (admin) user once if that has not happened yet.
pub async fn connect_with_admin(
    database_url: &str,
    admin_name: Option<&str>,
) -> anyhow::Result<Db> {
    connect_with_migrator(database_url, admin_name, &sqlx::migrate!("../migrations")).await
}

/// [`connect_with_admin`] with an explicit migration set. The server always passes the embedded
/// one; tests pass the set another branch of the stack will have (for example with the durable
/// pairing limiter's version 0004 present) to prove the upgrade path on that ledger.
pub async fn connect_with_migrator(
    database_url: &str,
    admin_name: Option<&str>,
    migrator: &sqlx::migrate::Migrator,
) -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    // The owner declared experimental multi-user databases disposable. Never rewrite their
    // applied SQLx ledger to disguise settings003 as the default hygiene003.
    let old: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations')",
    )
    .fetch_one(&pool)
    .await?;
    if old {
        let checksum: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 3")
                .fetch_optional(&pool)
                .await?;
        anyhow::ensure!(checksum.as_ref().map(hex::encode).as_deref() != Some("1e89bcb129fcdeadb396a48b8310419532c080e8b4a6e7e329532d7fbed72074ac4a43f70bb0a389f16cc885343b865f"),
            "unsupported experimental settings003 ledger: preserve this database and recreate ONLY your positively identified disposable task-owned fixture with the current migrations; never edit _sqlx_migrations or delete production/reviewer databases");
    }
    migrator.run(&pool).await?;
    crate::users::reconcile_name_keys(&pool).await?;
    if let Some(name) = admin_name {
        crate::users::apply_admin_name(&pool, name).await?;
    }
    Ok(pool)
}

/// Fresh in-memory database for tests (single connection so all queries share it).
pub async fn connect_memory() -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("../migrations").run(&pool).await?;
    Ok(pool)
}
