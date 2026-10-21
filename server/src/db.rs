//! SQLite pool setup. WAL + foreign keys on, migrations embedded from `migrations/`.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

pub type Db = SqlitePool;

pub async fn connect(database_url: &str) -> anyhow::Result<Db> {
    connect_with_admin(database_url, None).await
}

/// Like [`connect`], and names the first (admin) user if it still has its placeholder name.
pub async fn connect_with_admin(database_url: &str, admin_name: Option<&str>) -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    sqlx::migrate!("../migrations").run(&pool).await?;
    ensure_epoch(&pool).await?;
    if let Some(name) = admin_name {
        crate::users::apply_admin_name(&pool, name).await?;
    }
    Ok(pool)
}

/// Give the database a random identity the first time it is opened and keep it afterwards.
/// Clients remember it: a different value means this is not the database they synced with
/// (rebuilt or replaced server), so they must start over. Stored in `meta` as an integer
/// small enough to survive JSON in any client.
pub async fn ensure_epoch(db: &Db) -> anyhow::Result<()> {
    use rand::Rng;
    let epoch: i64 = rand::thread_rng().gen_range(1..(1i64 << 48));
    sqlx::query("INSERT OR IGNORE INTO meta (key, val) VALUES ('epoch', ?)")
        .bind(epoch)
        .execute(db)
        .await?;
    Ok(())
}

/// Fresh in-memory database for tests (single connection so all queries share it).
pub async fn connect_memory() -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
    sqlx::migrate!("../migrations").run(&pool).await?;
    ensure_epoch(&pool).await?;
    Ok(pool)
}
