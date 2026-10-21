//! Users. Migration 0002 creates the first user, the admin, and gives it all pre-existing data.
//! Until requests are scoped to the calling user (a later change) every write is attributed to
//! that first user, so a single-user deployment behaves exactly as before.

use crate::db::Db;

/// The first user, created by migration 0002. Owns all data that existed before multi-user.
pub const ADMIN_ID: i64 = 1;

/// Name the migration gives the first user until `BREADTRACKER_ADMIN_NAME` renames it.
pub const DEFAULT_ADMIN_NAME: &str = "admin";

pub const MAX_NAME_LEN: usize = 40;

/// A user name: 1 to 40 characters, letters, digits, space, `.`, `_` and `-`, no leading or
/// trailing space. Uniqueness (case-insensitive) is enforced by the database.
pub fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=MAX_NAME_LEN).contains(&n)
        && name == name.trim()
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
}

/// Give the first user the configured name. Only renames while it still has the migration's
/// placeholder name, so a restart with a different value never renames an established admin.
/// Returns whether the name changed.
pub async fn apply_admin_name(db: &Db, name: &str) -> anyhow::Result<bool> {
    let name = name.trim();
    anyhow::ensure!(valid_name(name), "BREADTRACKER_ADMIN_NAME must be 1-{MAX_NAME_LEN} letters, digits, spaces, '.', '_' or '-'");
    let res = sqlx::query("UPDATE users SET name = ? WHERE id = ? AND name = ? AND name != ?")
        .bind(name)
        .bind(ADMIN_ID)
        .bind(DEFAULT_ADMIN_NAME)
        .bind(name)
        .execute(db)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
    pub created_at: i64,
    pub disabled_at: Option<i64>,
}

pub async fn list(db: &Db) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, is_admin, created_at, disabled_at FROM users ORDER BY id").fetch_all(db).await
}

/// Disable or enable a user without touching their data. A disabled user's devices are refused
/// (403) and they get no reminders. Admins cannot be disabled. Returns false when there is no
/// such (non-admin) user.
pub async fn set_disabled(db: &Db, id: i64, disabled: bool) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("UPDATE users SET disabled_at = ? WHERE id = ? AND is_admin = 0")
        .bind(if disabled { Some(crate::store::now_ms()) } else { None })
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Find a user by name (case-insensitive, as names are unique that way).
pub async fn find(db: &Db, name: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, is_admin, created_at, disabled_at FROM users WHERE name = ?")
        .bind(name.trim())
        .fetch_optional(db)
        .await
}

pub async fn find_by_id(db: &Db, id: i64) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, is_admin, created_at, disabled_at FROM users WHERE id = ?").bind(id).fetch_optional(db).await
}

/// What [`remove`] deleted, counted inside the same transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub name: String,
    pub devices: i64,
    pub items: i64,
    pub completions: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveError {
    NotFound,
    /// Admins cannot be removed: `pair` without a user, invite ownership and first-run setup
    /// all rely on them.
    IsAdmin,
}

/// Delete a user and everything they own in one transaction: devices (their tokens stop
/// working at once), items, completions, settings, reminder markers and pairing codes. Other
/// users are untouched. Invites keep their row; the reference to the user is cleared.
/// This is the one deletion path: the CLI uses it, and so will self-service account deletion.
pub async fn remove(db: &Db, user_id: i64) -> Result<Result<Removed, RemoveError>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let Some((name, is_admin)): Option<(String, bool)> =
        sqlx::query_as("SELECT name, is_admin FROM users WHERE id = ?").bind(user_id).fetch_optional(&mut *tx).await?
    else {
        return Ok(Err(RemoveError::NotFound));
    };
    if is_admin {
        return Ok(Err(RemoveError::IsAdmin));
    }
    // Table names come from this fixed list, never from input.
    let mut counts = [0i64; 3];
    for (i, sql) in [
        "SELECT COUNT(*) FROM devices WHERE user_id = ?",
        "SELECT COUNT(*) FROM items WHERE user_id = ?",
        "SELECT COUNT(*) FROM completions WHERE user_id = ?",
    ]
    .into_iter()
    .enumerate()
    {
        counts[i] = sqlx::query_scalar(sql).bind(user_id).fetch_one(&mut *tx).await?;
    }
    // Every owned table references users(id) ON DELETE CASCADE, so this removes it all.
    sqlx::query("DELETE FROM users WHERE id = ?").bind(user_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Ok(Removed { name, devices: counts[0], items: counts[1], completions: counts[2] }))
}

/// Everything stored for a user as one JSON document: the account, settings, devices (names
/// and times only, never tokens), items and completions with their tombstones. The server
/// route and the CLI both use this, so they cannot drift. `None` when the user does not exist.
pub async fn export(db: &Db, user_id: i64) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let Some(u) = find_by_id(db, user_id).await? else { return Ok(None) };
    let user = crate::store::UserDb::new(db, user_id);
    let settings = user.settings().await?;
    let devices: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT name, created_at, last_seen FROM devices WHERE user_id = ? ORDER BY created_at, id")
            .bind(user_id)
            .fetch_all(db)
            .await?;
    let mut items = user.all_items().await?;
    items.sort_by(|a, b| a.id.cmp(&b.id));
    let mut completions = user.all_completions().await?;
    completions.sort_by(|a, b| (&a.item_id, &a.date).cmp(&(&b.item_id, &b.date)));
    Ok(Some(serde_json::json!({
        "format": "bread-tracker-export",
        "version": 1,
        "exported_at": crate::store::now_ms(),
        "account": { "name": u.name, "created_at": u.created_at },
        "settings": {
            "timezone": settings.timezone,
            "quiet_start": settings.quiet_start,
            "quiet_end": settings.quiet_end,
            "reminders_enabled": settings.reminders_enabled,
        },
        "devices": devices
            .into_iter()
            .map(|(name, created_at, last_seen)| serde_json::json!({"name": name, "created_at": created_at, "last_seen": last_seen}))
            .collect::<Vec<_>>(),
        "items": items,
        "completions": completions,
    })))
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn names() {
        assert!(valid_name("Alex"));
        assert!(valid_name("a.b_c-d e"));
        assert!(!valid_name(""));
        assert!(!valid_name(" lead"));
        assert!(!valid_name("trail "));
        assert!(!valid_name("bad/char"));
        assert!(!valid_name(&"x".repeat(41)));
        assert!(valid_name(&"x".repeat(40)));
    }
}
