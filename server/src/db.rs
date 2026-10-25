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

    migrator.run(&pool).await?;
    crate::users::backfill_name_keys(&pool).await?;
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
