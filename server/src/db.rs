//! SQLite pool setup. WAL + foreign keys on, migrations embedded from `migrations/`.

use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use uuid::Uuid;

pub type Db = SqlitePool;

fn generation_path(database_path: &std::path::Path) -> std::path::PathBuf {
    database_path.with_extension("sync-generation")
}

fn pending_generation_path(database_path: &std::path::Path) -> std::path::PathBuf {
    database_path.with_extension("sync-generation-pending")
}

pub fn next_generation(database_path: &std::path::Path) -> anyhow::Result<String> {
    read_or_create_generation(database_path).map(|_| Uuid::new_v4().to_string())
}

pub struct GenerationLock(std::fs::File);

impl GenerationLock {
    pub fn acquire_exclusive(database_path: &std::path::Path) -> anyhow::Result<Self> {
        use fs2::FileExt;
        let lock_path = database_path.with_extension("sync-lock");
        if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.lock_exclusive()?;
        Ok(Self(file))
    }

    pub fn acquire_shared(database_path: &std::path::Path) -> anyhow::Result<Self> {
        Self::try_acquire_shared(database_path)
    }

    pub fn try_acquire_shared(database_path: &std::path::Path) -> anyhow::Result<Self> {
        let lock_path = database_path.with_extension("sync-lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.try_lock_shared()?;
        Ok(Self(file))
    }

    pub fn try_acquire_lifetime_exclusive(database_path: &std::path::Path) -> anyhow::Result<Self> {
        Self::try_acquire_exclusive(database_path)
    }

    pub fn try_acquire_exclusive(database_path: &std::path::Path) -> anyhow::Result<Self> {
        use fs2::FileExt;
        let lock_path = database_path.with_extension("sync-lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.try_lock_exclusive()?;
        Ok(Self(file))
    }
}

impl Drop for GenerationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Atomically rotate the durable generation kept outside the SQLite database and its backups.
pub fn rotate_sidecar_generation(database_path: &std::path::Path) -> anyhow::Result<String> {
    use std::io::Write;
    let sidecar = generation_path(database_path);
    let parent = sidecar
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = sidecar.with_extension(format!("generation.{}.tmp", uuid::Uuid::new_v4()));
    let generation = uuid::Uuid::new_v4().to_string();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    writeln!(file, "{generation}")?;
    file.sync_all()?;
    publish_prepared_generation(&temp, &sidecar, parent)?;
    Ok(generation)
}

fn publish_prepared_generation(
    temp: &std::path::Path,
    sidecar: &std::path::Path,
    parent: &std::path::Path,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    if sidecar.exists() {
        std::fs::remove_file(sidecar)?;
    }
    std::fs::rename(temp, sidecar)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn read_or_create_generation(database_path: &std::path::Path) -> anyhow::Result<String> {
    let path = generation_path(database_path);
    match std::fs::read_to_string(&path) {
        Ok(value) if uuid::Uuid::parse_str(value.trim()).is_ok() => Ok(value.trim().to_owned()),
        Ok(_) => anyhow::bail!(
            "invalid sync generation sidecar {}; use mark-restored to rotate it",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            rotate_sidecar_generation(database_path)
        }
        Err(e) => Err(e.into()),
    }
}

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
/// SQLite result codes for `SQLITE_BUSY` and `SQLITE_LOCKED`, plus their extended forms.
/// Extended codes are reported when the connection enables them, and the lock can appear
/// either directly or wrapped by the migrator.
const SQLITE_BUSY_CODES: [&str; 8] = ["5", "261", "517", "773", "6", "262", "518", "774"];

/// Attempts for one operation that SQLite reports busy or locked for after its own
/// `busy_timeout` has already elapsed. Two attempts means a single retry.
///
/// The bound is shared by the sync transaction, request admission and the informational
/// last-seen update, so a caller must not wrap another retry around it.
const BUSY_RETRY_ATTEMPTS: usize = 2;
/// Delay before the single busy retry, short enough to stay inside a client's request timeout.
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(100);

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
        // TRACK-82: the rejected PR49 head shipped a 0005 that reset the owner to UTC from the
        // current device count. Nothing can recover a value it overwrote, and its checksum is
        // not the published 0005, so SQLx would refuse with a generic mismatch. Name it.
        let neutral_timezone_checksum: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 5")
                .fetch_optional(pool)
                .await?;
        anyhow::ensure!(neutral_timezone_checksum.as_ref().map(hex::encode).as_deref() != Some("401b8424ab749e4d1644fee4ea86f401143869ed8e2f227f46475ae519bf92b5fd59c3106f4be298bc57f92f58d7ae11"),
            "unsupported experimental neutral-timezone 0005 ledger: this database recorded the rejected migration that reset an owner to UTC from the device count alone, and that value cannot be reconstructed; preserve this database and recreate ONLY your positively identified disposable task-owned fixture with the current migrations");
    }
    record_install_provenance(pool).await?;
    migrator.run(pool).await?;
    crate::users::reconcile_name_keys(pool).await?;
    if let Some(name) = admin_name {
        crate::users::apply_admin_name(pool, name).await?;
    }
    Ok(())
}

/// Record, once and before any migration applies, whether this database was an empty file.
///
/// This is the only reliable fresh-vs-populated provenance (TRACK-82). The current device count
/// is not: a household can revoke or lose every device and still keep its timezone, quiet hours
/// and reminder history. Only the "fresh" answer needs recording here; an existing database
/// without a marker is classified as populated by migration 0012, which keeps whatever 0007
/// adopted from it. Recording before the migration run also means a first start that fails
/// inside 0001 (rolled back, but the marker table already committed) is still fresh on retry
/// instead of being mistaken for a populated schema.
async fn record_install_provenance(pool: &Db) -> anyhow::Result<()> {
    let marker_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='install_provenance')",
    )
    .fetch_one(pool)
    .await?;
    let recorded: Option<String> = if marker_exists {
        sqlx::query_scalar("SELECT value FROM install_provenance WHERE key = 'install_kind'")
            .fetch_optional(pool)
            .await?
    } else {
        None
    };
    if recorded.is_some() {
        return Ok(());
    }
    let empty: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' \
         AND name NOT LIKE 'sqlite_%' AND name <> 'install_provenance')",
    )
    .fetch_one(pool)
    .await?;
    if !empty {
        // Existing database: 0012 records it as populated and preserves its settings.
        return Ok(());
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS install_provenance (\
           key TEXT PRIMARY KEY, \
           value TEXT NOT NULL CHECK (value IN ('fresh', 'populated'))\
         )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO install_provenance (key, value) VALUES ('install_kind', 'fresh')",
    )
    .execute(pool)
    .await?;
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

/// Run `operation` and retry it once when it fails because SQLite is busy or locked.
///
/// `operation` must start from scratch on every attempt: a failed `BEGIN IMMEDIATE`
/// transaction is rolled back when it is dropped, so no partial write from the first attempt
/// can survive into the retry. The one-retry bound is shared with the other callers, so this
/// helper must not be nested inside another retry.
pub(crate) async fn retry_busy<T, E, F, Fut>(
    is_busy: fn(&E) -> bool,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        match operation().await {
            Err(err) if attempt < BUSY_RETRY_ATTEMPTS && is_busy(&err) => {
                tracing::warn!(attempt, "database is busy; retrying once");
                tokio::time::sleep(BUSY_RETRY_DELAY).await;
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// Open an existing database for the restore marker without creating a fresh one. The
/// migrations run at startup still apply, so a database restored from an older replica is
/// brought forward before the generation journal is written.
pub async fn connect_existing(database_url: &str) -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    prepare(&pool, None, &sqlx::migrate!("../migrations")).await?;
    Ok(pool)
}

pub async fn current_epoch(db: &Db) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT val FROM meta WHERE key = 'sync_epoch'")
        .fetch_optional(db)
        .await
}

pub async fn set_epoch(db: &Db, epoch: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO meta (key, val) VALUES ('sync_epoch', ?) ON CONFLICT(key) DO UPDATE SET val = excluded.val").bind(epoch).execute(db).await?;
    Ok(())
}

/// Reconcile the generation journal and SQLite row before requests are served.
pub async fn reconcile_generation(
    db: &Db,
    database_path: &std::path::Path,
) -> anyhow::Result<String> {
    let _lock = GenerationLock::acquire_exclusive(database_path)?;
    reconcile_generation_locked(db, database_path).await
}

pub async fn reconcile_generation_locked(
    db: &Db,
    database_path: &std::path::Path,
) -> anyhow::Result<String> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let pending_path = pending_generation_path(database_path);
    let pending = match std::fs::read_to_string(&pending_path) {
        Ok(value) if Uuid::parse_str(value.trim()).is_ok() => Some(value.trim().to_owned()),
        Ok(_) => anyhow::bail!("invalid pending sync generation; refusing startup"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    // Always publish the selected generation, not only when a journal exists. This
    // repairs a crash after publication but before journal cleanup as well as ordinary
    // first-start initialization.
    let generation = if let Some(pending) = pending {
        pending
    } else {
        match std::fs::read_to_string(generation_path(database_path)) {
            Ok(value) if uuid::Uuid::parse_str(value.trim()).is_ok() => value.trim().to_owned(),
            Ok(_) => anyhow::bail!("invalid sync generation sidecar; refusing startup"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let db_generation: Option<String> =
                    sqlx::query_scalar("SELECT val FROM meta WHERE key = 'sync_epoch'")
                        .fetch_optional(&mut *tx)
                        .await?;
                if let Some(existing) = db_generation {
                    publish_generation(database_path, &existing)?;
                    existing
                } else {
                    let fresh = uuid::Uuid::new_v4().to_string();
                    publish_generation(database_path, &fresh)?;
                    fresh
                }
            }
            Err(error) => return Err(error.into()),
        }
    };
    sqlx::query("INSERT INTO meta (key, val) VALUES ('sync_epoch', ?) ON CONFLICT(key) DO UPDATE SET val = excluded.val")
        .bind(&generation)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    publish_generation(database_path, &generation)?;
    remove_pending_generation(database_path)?;
    Ok(generation)
}

/// Production restore-marker operation. The caller must hold the exclusive generation
/// lock and stop the normal server before invoking this against the restored database.
pub async fn mark_restored(
    db: &Db,
    database_path: &std::path::Path,
    generation: &str,
) -> anyhow::Result<()> {
    mark_restored_inner(
        db,
        database_path,
        generation,
        || {
            if std::env::var("BREADTRACKER_INJECT_MARK_FAILURE").as_deref() == Ok("after-journal") {
                anyhow::bail!("injected restore marker failure after journal publication");
            }
            Ok(())
        },
        || Ok(()),
    )
    .await
}

#[cfg(test)]
fn apply_restore_snapshot_to_values<T, K, FKey, FUpdated, FMerge>(
    local: &mut std::collections::HashMap<K, T>,
    incoming: impl IntoIterator<Item = T>,
    key: FKey,
    updated_at: FUpdated,
    merge: FMerge,
) where
    K: std::hash::Hash + Eq,
    T: Clone,
    FKey: Fn(&T) -> K,
    FUpdated: Fn(&T) -> i64,
    FMerge: Fn(&mut T, &T),
{
    for row in incoming {
        let k = key(&row);
        if let Some(existing) = local.get_mut(&k) {
            if updated_at(existing) < updated_at(&row) {
                merge(existing, &row);
            }
        } else {
            local.insert(k, row);
        }
    }
}

async fn mark_restored_inner<F>(
    db: &Db,
    database_path: &std::path::Path,
    generation: &str,
    after_journal: F,
    after_commit: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()>,
{
    use std::io::Write;
    let sidecar = generation_path(database_path);
    let pending = pending_generation_path(database_path);
    let parent = sidecar
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = sidecar.with_extension(format!("generation.{}.tmp", Uuid::new_v4()));
    let journal_temp = pending.with_extension(format!("generation.{}.tmp", Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    writeln!(file, "{generation}")?;
    file.sync_all()?;
    let mut journal = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&journal_temp)?;
    writeln!(journal, "{generation}")?;
    journal.sync_all()?;
    publish_prepared_generation(&journal_temp, &pending, parent)?;
    after_journal()?;
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("INSERT INTO meta (key, val) VALUES ('sync_epoch', ?) ON CONFLICT(key) DO UPDATE SET val = excluded.val")
        .bind(generation)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // The durable pending journal makes the new value recoverable if the process stops
    // after the DB commit but before the current sidecar is published.
    after_commit()?;
    publish_prepared_generation(&temp, &sidecar, parent)?;
    remove_pending_generation(database_path)?;
    Ok(())
}

fn remove_pending_generation(database_path: &std::path::Path) -> anyhow::Result<()> {
    let path = pending_generation_path(database_path);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn publish_generation(database_path: &std::path::Path, generation: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let sidecar = generation_path(database_path);
    let parent = sidecar
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = sidecar.with_extension(format!("generation.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    writeln!(file, "{generation}")?;
    file.sync_all()?;
    #[cfg(windows)]
    if sidecar.exists() {
        std::fs::remove_file(&sidecar)?;
    }
    std::fs::rename(&temp, &sidecar)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
/// Fresh in-memory database for tests (single connection so all queries share it).
pub async fn connect_memory() -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    prepare(&pool, None, &sqlx::migrate!("../migrations")).await?;
    Ok(pool)
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    #[test]
    fn sidecar_initializes_persists_and_rotates_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("tracker.db");
        let first = read_or_create_generation(&database).unwrap();
        assert_eq!(first, read_or_create_generation(&database).unwrap());
        let rotated = rotate_sidecar_generation(&database).unwrap();
        assert_ne!(first, rotated);
        assert_eq!(rotated, read_or_create_generation(&database).unwrap());
    }

    #[tokio::test]
    async fn startup_uses_existing_generation_without_rotating_it() {
        let dir = tempfile::tempdir().unwrap();
        let database_path = dir.path().join("tracker.db");
        std::fs::write(&database_path, b"existing database marker").unwrap();
        let sidecar_generation = read_or_create_generation(&database_path).unwrap();
        let db = connect_memory().await.unwrap();
        set_epoch(&db, &sidecar_generation).await.unwrap();
        let effective = read_or_create_generation(&database_path).unwrap();
        assert_eq!(effective, sidecar_generation);
        assert_eq!(
            current_epoch(&db).await.unwrap().as_deref(),
            Some(effective.as_str())
        );
    }

    #[tokio::test]
    async fn existing_populated_database_keeps_rows_and_uses_external_generation() {
        let db = connect_memory().await.unwrap();
        sqlx::query("INSERT INTO items (user_id,id,kind,name,weekdays,points,sort,active,updated_at,seq) VALUES (1,'id','chore','kept',127,10,0,1,1,1)")
            .execute(&db).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE name='kept'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.db");
        let generation = read_or_create_generation(&path).unwrap();
        set_epoch(&db, &generation).await.unwrap();
        assert_eq!(generation, read_or_create_generation(&path).unwrap());
        let still_there: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE name='kept'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(still_there, 1);
    }

    #[tokio::test]
    async fn forward_generation_initialization_preserves_populated_legacy_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let database_url = format!("sqlite://{}", path.display());
        let legacy = SqliteConnectOptions::from_str(&database_url)
            .unwrap()
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let legacy_db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(legacy)
            .await
            .unwrap();
        sqlx::migrate!("../migrations")
            .run(&legacy_db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO items (user_id,id,kind,name,weekdays,points,sort,active,updated_at,seq) VALUES (1,'legacy','chore','preserved',127,10,0,1,1,1)")
            .execute(&legacy_db).await.unwrap();
        legacy_db.close().await;

        let upgraded = connect_existing(&database_url).await.unwrap();
        reconcile_generation_locked(&upgraded, &path).await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM items WHERE id='legacy'")
            .fetch_one(&upgraded)
            .await
            .unwrap();
        assert_eq!(name, "preserved");
        assert!(current_epoch(&upgraded).await.unwrap().is_some());
        upgraded.close().await;
    }

    #[test]
    fn restore_lock_waits_for_running_server_lock() {
        use fs2::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let server = GenerationLock::try_acquire_shared(&path).unwrap();
        let marker = path.with_extension("sync-lock");
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(marker)
            .unwrap();
        assert!(contender.try_lock_exclusive().is_err());
        drop(server);
        contender.try_lock_exclusive().unwrap();
        FileExt::unlock(&contender).unwrap();
    }

    #[test]
    fn restore_lock_reports_running_server_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let _server = GenerationLock::try_acquire_shared(&path).unwrap();
        assert!(GenerationLock::try_acquire_lifetime_exclusive(&path).is_err());
    }

    #[test]
    fn pair_shared_lock_coexists_with_running_server_and_excludes_restore() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let _server = GenerationLock::try_acquire_shared(&path).unwrap();
        let _pair = GenerationLock::try_acquire_shared(&path).unwrap();
        assert!(GenerationLock::try_acquire_exclusive(&path).is_err());
    }

    #[tokio::test]
    async fn restore_marker_conflicts_with_server_lifetime_shared_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let _server = GenerationLock::try_acquire_shared(&path).unwrap();
        assert!(GenerationLock::try_acquire_lifetime_exclusive(&path).is_err());
    }

    #[tokio::test]
    async fn generation_reconcile_rolls_forward_db_after_crash_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let first = read_or_create_generation(&path).unwrap();
        let db = connect_memory().await.unwrap();
        set_epoch(&db, &first).await.unwrap();
        let published = rotate_sidecar_generation(&path).unwrap();
        assert_ne!(first, published);
        let recovered = reconcile_generation(&db, &path).await.unwrap();
        assert_eq!(recovered, published);
        assert_eq!(
            current_epoch(&db).await.unwrap().as_deref(),
            Some(published.as_str())
        );
    }

    #[tokio::test]
    async fn production_mark_restored_publishes_matching_sidecar_and_database_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let db = connect_memory().await.unwrap();
        let old = read_or_create_generation(&path).unwrap();
        set_epoch(&db, &old).await.unwrap();
        let _lock = GenerationLock::try_acquire_exclusive(&path).unwrap();
        let next = uuid::Uuid::new_v4().to_string();
        mark_restored(&db, &path, &next).await.unwrap();
        assert_ne!(old, next);
        assert_eq!(read_or_create_generation(&path).unwrap(), next);
        assert_eq!(
            current_epoch(&db).await.unwrap().as_deref(),
            Some(next.as_str())
        );
    }

    #[tokio::test]
    async fn repeated_restore_mark_is_idempotent_after_interrupted_db_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let db = connect_memory().await.unwrap();
        let old = read_or_create_generation(&path).unwrap();
        set_epoch(&db, &old).await.unwrap();
        let _lock = GenerationLock::try_acquire_exclusive(&path).unwrap();
        let first = uuid::Uuid::new_v4().to_string();
        mark_restored_inner(
            &db,
            &path,
            &first,
            || Ok(()),
            || anyhow::bail!("injected crash after commit"),
        )
        .await
        .unwrap_err();
        let second = uuid::Uuid::new_v4().to_string();
        mark_restored(&db, &path, &second).await.unwrap();
        assert_ne!(old, first);
        assert_ne!(old, second);
        assert_eq!(read_or_create_generation(&path).unwrap(), second);
        assert_eq!(
            current_epoch(&db).await.unwrap().as_deref(),
            Some(second.as_str())
        );
    }

    #[tokio::test]
    async fn crash_before_sidecar_publication_recovers_pending_generation_before_serving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let old = read_or_create_generation(&path).unwrap();
        let db = connect_memory().await.unwrap();
        set_epoch(&db, &old).await.unwrap();
        let _lock = GenerationLock::try_acquire_exclusive(&path).unwrap();
        let proposed = Uuid::new_v4().to_string();
        mark_restored_inner(
            &db,
            &path,
            &proposed,
            || Ok(()),
            || anyhow::bail!("injected crash"),
        )
        .await
        .unwrap_err();
        // Simulate termination after DB commit but before atomic rename.
        assert_eq!(read_or_create_generation(&path).unwrap(), old);
        let recovered = reconcile_generation_locked(&db, &path).await.unwrap();
        assert_eq!(recovered, proposed);
        assert_eq!(
            current_epoch(&db).await.unwrap().as_deref(),
            Some(proposed.as_str())
        );
        assert_eq!(read_or_create_generation(&path).unwrap(), proposed);
    }

    #[tokio::test]
    async fn crash_after_pending_journal_before_database_commit_keeps_old_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracker.db");
        let db = connect_memory().await.unwrap();
        let old = read_or_create_generation(&path).unwrap();
        set_epoch(&db, &old).await.unwrap();
        let _lock = GenerationLock::try_acquire_exclusive(&path).unwrap();
        let proposed = Uuid::new_v4().to_string();
        mark_restored_inner(
            &db,
            &path,
            &proposed,
            || anyhow::bail!("injected crash after pending journal"),
            || Ok(()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            current_epoch(&db).await.unwrap().as_deref(),
            Some(old.as_str())
        );
        let recovered = reconcile_generation_locked(&db, &path).await.unwrap();
        assert_eq!(recovered, proposed);
        assert_eq!(
            current_epoch(&db).await.unwrap().as_deref(),
            Some(proposed.as_str())
        );
    }

    #[test]
    fn restore_snapshot_value_merge_preserves_newer_clean_item_and_tombstone() {
        let mut items = std::collections::HashMap::from([(
            "item".to_owned(),
            crate::model::Item {
                id: "item".into(),
                kind: "chore".into(),
                name: "new synced name".into(),
                weekdays: 127,
                remind_time: None,
                points: 10,
                due_date: None,
                sort: 0,
                active: true,
                updated_at: 2000,
                deleted_at: None,
                seq: 3,
            },
        )]);
        let backup = crate::model::Item {
            id: "item".into(),
            kind: "chore".into(),
            name: "backup old".into(),
            weekdays: 127,
            remind_time: None,
            points: 10,
            due_date: None,
            sort: 0,
            active: true,
            updated_at: 1000,
            deleted_at: None,
            seq: 1,
        };
        apply_restore_snapshot_to_values(
            &mut items,
            [backup],
            |r| r.id.clone(),
            |r| r.updated_at,
            |a, b| *a = b.clone(),
        );
        assert_eq!(items.get("item").unwrap().name, "new synced name");
        let mut completions = std::collections::HashMap::from([(
            ("item".to_owned(), "2026-10-19".to_owned()),
            crate::model::Completion {
                item_id: "item".into(),
                date: "2026-10-19".into(),
                completed_at: 0,
                updated_at: 2100,
                deleted_at: Some(2100),
                seq: 4,
            },
        )]);
        let backup_completion = crate::model::Completion {
            item_id: "item".into(),
            date: "2026-10-19".into(),
            completed_at: 1100,
            updated_at: 1100,
            deleted_at: None,
            seq: 2,
        };
        apply_restore_snapshot_to_values(
            &mut completions,
            [backup_completion],
            |r| (r.item_id.clone(), r.date.clone()),
            |r| r.updated_at,
            |a, b| *a = b.clone(),
        );
        assert_eq!(completions.values().next().unwrap().deleted_at, Some(2100));
    }
}
