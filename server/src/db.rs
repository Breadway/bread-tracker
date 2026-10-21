//! SQLite pool setup. WAL + foreign keys on, migrations embedded from `migrations/`.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

pub type Db = SqlitePool;

fn generation_path(database_path: &std::path::Path) -> std::path::PathBuf {
    database_path.with_extension("sync-generation")
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
    std::fs::rename(&temp, &sidecar)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(generation)
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

pub async fn connect(database_url: &str) -> anyhow::Result<Db> {
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
    Ok(pool)
}

/// Keep a database-local identity for old clients and new clients talking to old servers.
pub async fn ensure_epoch(db: &Db) -> anyhow::Result<()> {
    let existing: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'sync_epoch'")
            .fetch_optional(db)
            .await?;
    if existing.is_none() {
        let epoch = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO settings (key, value) VALUES ('sync_epoch', ?) ON CONFLICT(key) DO NOTHING").bind(epoch).execute(db).await?;
    }
    Ok(())
}

pub async fn set_epoch(db: &Db, epoch: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO settings (key, value) VALUES ('sync_epoch', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value").bind(epoch).execute(db).await?;
    Ok(())
}

pub async fn connect_memory() -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("../migrations").run(&pool).await?;
    ensure_epoch(&pool).await?;
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
    async fn existing_populated_database_keeps_rows_and_uses_external_generation() {
        let db = connect_memory().await.unwrap();
        sqlx::query("INSERT INTO items (id,kind,name,weekdays,points,sort,active,updated_at,seq) VALUES ('id','chore','kept',127,10,0,1,1,1)")
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
}
