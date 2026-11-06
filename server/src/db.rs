//! SQLite pool setup. WAL + foreign keys on, migrations embedded from `migrations/`.

use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

pub type Db = SqlitePool;

/// How long opening and preparing the database keeps retrying while SQLite reports BUSY.
///
/// SQLite's WAL open/recovery and journal-mode locks do not go through the connection busy
/// handler, so a database being checkpointed by another connection (or by Litestream after a
/// restart) can fail the first attempt immediately even with `busy_timeout` set. The open path
/// is idempotent (migrations run in transactions, name reconciliation and the admin name are
/// guarded), so retrying it is safe.
const OPEN_RETRY_BUDGET: Duration = Duration::from_secs(30);
/// First backoff between attempts; doubles up to [`OPEN_RETRY_MAX`].
const OPEN_RETRY_FIRST: Duration = Duration::from_millis(250);
/// Upper bound on a single backoff between open attempts.
const OPEN_RETRY_MAX: Duration = Duration::from_secs(2);
/// SQLite primary result code for `SQLITE_BUSY`, plus the extended busy codes that can be
/// reported when extended result codes are enabled.
const SQLITE_BUSY_CODES: [&str; 4] = ["5", "261", "517", "773"];

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
///
/// A busy database is retried with bounded backoff. A close/reopen of the same file in one
/// process is not what the server does, but a startup can race a WAL checkpoint left by the
/// previous process or by Litestream, and that lock bypasses `busy_timeout`.
pub async fn connect_with_migrator(
    database_url: &str,
    admin_name: Option<&str>,
    migrator: &sqlx::migrate::Migrator,
) -> anyhow::Result<Db> {
    let deadline = tokio::time::Instant::now() + OPEN_RETRY_BUDGET;
    let mut delay = OPEN_RETRY_FIRST;
    loop {
        match open_and_prepare(database_url, admin_name, migrator).await {
            Ok(pool) => return Ok(pool),
            Err(err) if is_sqlite_busy(&err) && tokio::time::Instant::now() < deadline => {
                tracing::warn!(
                    %err,
                    "database is busy while opening; retrying after {delay:?}"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(OPEN_RETRY_MAX);
            }
            Err(err) => return Err(err),
        }
    }
}

/// One open-and-prepare attempt. On any failure after the pool exists it closes the partial
/// pool so the next attempt starts from a clean handle.
async fn open_and_prepare(
    database_url: &str,
    admin_name: Option<&str>,
    migrator: &sqlx::migrate::Migrator,
) -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    let prepared = prepare(&pool, admin_name, migrator).await;
    match prepared {
        Ok(()) => Ok(pool),
        Err(err) => {
            pool.close().await;
            Err(err)
        }
    }
}

async fn prepare(
    pool: &Db,
    admin_name: Option<&str>,
    migrator: &sqlx::migrate::Migrator,
) -> anyhow::Result<()> {
    // The owner declared experimental multi-user databases disposable. Never rewrite their
    // applied SQLx ledger to disguise settings003 as the default hygiene003.
    let old: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    // Establish freshness from the schema before migrations, never from the
    // current device count. Revoked devices do not make an old household new.
    let fresh: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')",
    ).fetch_one(pool).await?;
    if old {
        let checksum: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 3")
                .fetch_optional(pool)
                .await?;
        anyhow::ensure!(checksum.as_ref().map(hex::encode).as_deref() != Some("1e89bcb129fcdeadb396a48b8310419532c080e8b4a6e7e329532d7fbed72074ac4a43f70bb0a389f16cc885343b865f"),
            "unsupported experimental settings003 ledger: preserve this database and recreate ONLY your positively identified disposable task-owned fixture with the current migrations; never edit _sqlx_migrations or delete production/reviewer databases");
        let invite_checksum: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 4")
                .fetch_optional(pool)
                .await?;
        anyhow::ensure!(invite_checksum.as_ref().map(hex::encode).as_deref() != Some("ea989dda2ed972c914b78d423bc7dff9f81f5043252d7b616c287c8cbd82c1645ff96462022c625bd3e286c110bb1ed6"),
            "unsupported experimental invites004 ledger: preserve this database and recreate ONLY your positively identified disposable task-owned fixture; never rewrite the ledger or remove production/reviewer data");
    }
    migrator.run(pool).await?;
    if fresh {
        sqlx::query("UPDATE user_settings SET timezone = 'UTC' WHERE user_id = 1")
            .execute(pool)
            .await?;
    }
    crate::users::reconcile_name_keys(pool).await?;
    if let Some(name) = admin_name {
        crate::users::apply_admin_name(pool, name).await?;
    }
    Ok(())
}

/// Whether any layer of `err` is SQLite reporting that the database is locked.
///
/// The busy error can come directly from the pool, from a statement, or wrapped by the SQLx
/// migrator, so the whole error chain is inspected rather than the rendered message.
fn is_sqlite_busy(err: &anyhow::Error) -> bool {
    use sqlx::migrate::MigrateError;

    err.chain().any(|cause| {
        if let Some(sqlx_err) = cause.downcast_ref::<sqlx::Error>() {
            return is_busy_database_error(sqlx_err);
        }
        if let Some(migrate_err) = cause.downcast_ref::<MigrateError>() {
            return match migrate_err {
                MigrateError::Execute(inner) => is_busy_database_error(inner),
                MigrateError::ExecuteMigration(inner, _) => is_busy_database_error(inner),
                _ => false,
            };
        }
        false
    })
}

pub(crate) fn is_busy_database_error(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db) => db
            .code()
            .as_deref()
            .is_some_and(|code| SQLITE_BUSY_CODES.contains(&code)),
        _ => false,
    }
}

/// Fresh in-memory database for tests (single connection so all queries share it).
pub async fn connect_memory() -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("../migrations").run(&pool).await?;
    sqlx::query("UPDATE user_settings SET timezone = 'UTC' WHERE user_id = 1")
        .execute(&pool)
        .await?;
    Ok(pool)
}
