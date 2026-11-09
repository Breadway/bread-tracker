//! Users. Migration 0002 creates the first user, the admin, and gives it all pre-existing data.
//! Until requests are scoped to the calling user (a later change) every write is attributed to
//! that first user, so a single-user deployment behaves exactly as before.
//!
//! Name policy (unchanged since 0002): 1 to 40 characters, Unicode letters and digits, space,
//! `.`, `_` and `-`, no leading or trailing space. Two names are the same user name when their
//! caseless keys match, using Unicode default case folding, so `Älex` and `älex` collide just
//! like `Alex` and `alex`. Every path that creates, renames or looks up a user goes through
//! [`name_key`], and the database enforces it with a unique index on `users.name_key`.

use caseless::default_case_fold_str;
use unicode_normalization::UnicodeNormalization;

use crate::db::Db;

/// The first user, created by migration 0002. Owns all data that existed before multi-user.
pub const ADMIN_ID: i64 = 1;

/// Name the migration gives the first user until `BREADTRACKER_ADMIN_NAME` names it.
pub const DEFAULT_ADMIN_NAME: &str = "admin";

pub const MAX_NAME_LEN: usize = 40;

/// A user name: 1 to 40 characters, letters, digits, space, `.`, `_` and `-`, no leading or
/// trailing space. Uniqueness is by [`name_key`] and is enforced by the database.
pub fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=MAX_NAME_LEN).contains(&n)
        && name == name.trim()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'))
}

/// The caseless identity of a name: canonical decomposition, Unicode default case folding,
/// decomposition again (the canonical caseless matching of the Unicode standard, section 3.13).
/// Two names are the same user name exactly when their keys are equal.
///
/// For printable ASCII this is plain lower case, which is what migration 0005 computes in SQL
/// for names that already exist.
pub fn name_key(name: &str) -> String {
    let decomposed: String = name.nfd().collect();
    default_case_fold_str(&decomposed).nfd().collect()
}

#[derive(Debug, thiserror::Error)]
pub enum NameError {
    #[error("invalid user name: use 1-{MAX_NAME_LEN} letters, digits, spaces, '.', '_' or '-'")]
    Invalid,
    #[error("a user with that name already exists (names are compared ignoring case)")]
    Taken,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

fn map_unique(e: sqlx::Error) -> NameError {
    match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => NameError::Taken,
        _ => NameError::Db(e),
    }
}

/// Create a user. Returns the new id.
pub async fn create(db: &Db, name: &str, is_admin: bool) -> Result<i64, NameError> {
    if !valid_name(name) {
        return Err(NameError::Invalid);
    }
    sqlx::query_scalar(
        "INSERT INTO users (name, name_key, name_confirmed, is_admin, created_at) \
         VALUES (?, ?, 1, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(name_key(name))
    .bind(is_admin)
    .bind(crate::store::now_ms())
    .fetch_one(db)
    .await
    .map_err(map_unique)
}

/// Rename a user. The name counts as chosen from then on, so the admin placeholder logic
/// never touches it again.
///
/// The key is cleared and set again inside one transaction. The database refuses a non-ASCII
/// rename that leaves the key as it was (a plain SQL rename would leave it stale), and a
/// case-only change such as `Älex` to `älex` has the same key before and after, so the
/// application path makes the change of key explicit instead of weakening that guard.
pub async fn rename(db: &Db, id: i64, name: &str) -> Result<bool, NameError> {
    if !valid_name(name) {
        return Err(NameError::Invalid);
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE users SET name_key = NULL WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let res =
        sqlx::query("UPDATE users SET name = ?, name_key = ?, name_confirmed = 1 WHERE id = ?")
            .bind(name)
            .bind(name_key(name))
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(map_unique)?;
    tx.commit().await?;
    Ok(res.rows_affected() > 0)
}

/// Find a user by name, with the same caseless comparison that uniqueness uses.
pub async fn find_by_name(db: &Db, name: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, is_admin, created_at, disabled_at FROM users WHERE name_key = ?",
    )
    .bind(name_key(name))
    .fetch_optional(db)
    .await
}

/// Name the first user from `BREADTRACKER_ADMIN_NAME`. Applied at most once: it only acts while
/// the first user still has the untouched placeholder name (exactly `admin`, byte for byte)
/// and has not been named yet (`name_confirmed = 0`). Choosing any name, including `Admin` or
/// `admin`, marks it confirmed, so restarting with the same or another value never changes an
/// established name. Returns whether the name was applied.
pub async fn apply_admin_name(db: &Db, name: &str) -> anyhow::Result<bool> {
    let name = name.trim();
    anyhow::ensure!(
        valid_name(name),
        "BREADTRACKER_ADMIN_NAME must be 1-{MAX_NAME_LEN} letters, digits, spaces, '.', '_' or '-'"
    );
    let res = sqlx::query(
        "UPDATE users SET name = ?, name_key = ?, name_confirmed = 1 \
         WHERE id = ? AND name_confirmed = 0 AND name = ? COLLATE BINARY",
    )
    .bind(name)
    .bind(name_key(name))
    .bind(ADMIN_ID)
    .bind(DEFAULT_ADMIN_NAME)
    .execute(db)
    .await
    .map_err(|e| match map_unique(e) {
        NameError::Taken => anyhow::anyhow!(
            "BREADTRACKER_ADMIN_NAME {name:?} is already used by another user (names are compared ignoring case)"
        ),
        other => anyhow::Error::new(other),
    })?;
    Ok(res.rows_affected() > 0)
}

/// What [`reconcile_name_keys`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Users that had no key (written without one, or the operator cleared it).
    pub filled: usize,
    /// Users whose stored key no longer matched their name (renamed with plain SQL).
    pub corrected: usize,
}

/// Check every user's `name_key` against their name and repair what is wrong, in one
/// `BEGIN IMMEDIATE` transaction. Runs at every start, before the listener and the scheduler.
///
/// SQLite cannot case fold Unicode, so a key can be missing (a non-ASCII name written without
/// one) or stale (a raw `UPDATE users SET name = ...` that changed the name but not the key).
/// This recomputes the key of every user from their current name. If a user's recomputed key is
/// already the key of another user, nothing is changed and startup fails naming both users:
/// which of them keeps the name is the operator's decision, never an automatic overwrite. The
/// remedy is to rename one while the server is stopped, with
/// `UPDATE users SET name = '<new name>', name_key = NULL WHERE id = <id>`; the key is
/// recomputed at the next start.
///
/// Keys are applied in two passes inside the transaction (cleared first, then set), so users
/// that merely swap keys do not trip the unique index halfway through.
pub async fn reconcile_name_keys(db: &Db) -> anyhow::Result<Reconciled> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, name, name_key FROM users ORDER BY id")
            .fetch_all(&mut *tx)
            .await?;

    let mut wanted: std::collections::HashMap<String, (i64, &str)> =
        std::collections::HashMap::new();
    for (id, name, _) in &rows {
        let key = name_key(name);
        if let Some((other_id, other_name)) = wanted.insert(key, (*id, name.as_str())) {
            anyhow::bail!(
                "users {other_id} ({other_name:?}) and {id} ({name:?}) have the same name ignoring case; \
                 rename one (UPDATE users SET name = '<new name>', name_key = NULL WHERE id = {id}) \
                 while the server is stopped, then start again"
            );
        }
    }

    let mut out = Reconciled::default();
    let changed: Vec<(i64, String)> = rows
        .iter()
        .filter_map(|(id, name, stored)| {
            let key = name_key(name);
            match stored {
                Some(s) if *s == key => None,
                Some(_) => {
                    out.corrected += 1;
                    Some((*id, key))
                }
                None => {
                    out.filled += 1;
                    Some((*id, key))
                }
            }
        })
        .collect();
    for (id, _) in &changed {
        sqlx::query("UPDATE users SET name_key = NULL WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    for (id, key) in &changed {
        sqlx::query("UPDATE users SET name_key = ? WHERE id = ?")
            .bind(key)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(out)
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
    sqlx::query_as("SELECT id, name, is_admin, created_at, disabled_at FROM users ORDER BY id")
        .fetch_all(db)
        .await
}

/// Disable or enable a user without touching their data. A disabled user's devices are refused
/// (403) and they get no reminders. Admins cannot be disabled. Returns false when there is no
/// such (non-admin) user.
pub async fn set_disabled(db: &Db, id: i64, disabled: bool) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("UPDATE users SET disabled_at = ? WHERE id = ? AND is_admin = 0")
        .bind(if disabled {
            Some(crate::store::now_ms())
        } else {
            None
        })
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Find a user by name (case-insensitive, as names are unique that way).
pub async fn find(db: &Db, name: &str) -> Result<Option<User>, sqlx::Error> {
    find_by_name(db, name).await
}

pub async fn find_by_id(db: &Db, id: i64) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, is_admin, created_at, disabled_at FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
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
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let Some((name, is_admin)): Option<(String, bool)> =
        sqlx::query_as("SELECT name, is_admin FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?
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
        counts[i] = sqlx::query_scalar(sql)
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    }
    // Every owned table references users(id) ON DELETE CASCADE, so this removes it all.
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Ok(Removed {
        name,
        devices: counts[0],
        items: counts[1],
        completions: counts[2],
    }))
}

/// Everything stored for a user as one JSON document: the account, settings, devices (names
/// and times only, never tokens), items and completions with their tombstones. The server
/// route and the CLI both use this, so they cannot drift. `None` when the user does not exist.
pub async fn export(db: &Db, user_id: i64) -> Result<Option<serde_json::Value>, sqlx::Error> {
    // All export sections come from one SQLite read snapshot, not independent
    // reads that can straddle a concurrent sync or deletion.
    let mut tx = db.begin().await?;
    let Some(u): Option<User> = sqlx::query_as(
        "SELECT id, name, is_admin, created_at, disabled_at FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let settings: crate::store::UserSettings = sqlx::query_as("SELECT timezone, quiet_start, quiet_end, reminders_enabled FROM user_settings WHERE user_id = ?")
        .bind(user_id).fetch_one(&mut *tx).await?;
    let devices: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT name, created_at, last_seen FROM devices WHERE user_id = ? ORDER BY created_at, id",
    )
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut items: Vec<crate::model::Item> = sqlx::query_as("SELECT id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, created_on, seq FROM items WHERE user_id = ?")
        .bind(user_id).fetch_all(&mut *tx).await?;
    items.sort_by(|a, b| a.id.cmp(&b.id));
    let mut completions: Vec<crate::model::Completion> = sqlx::query_as("SELECT item_id, date, completed_at, updated_at, deleted_at, seq FROM completions WHERE user_id = ?")
        .bind(user_id).fetch_all(&mut *tx).await?;
    completions.sort_by(|a, b| (&a.item_id, &a.date).cmp(&(&b.item_id, &b.date)));
    tx.commit().await?;
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
    use super::{name_key, valid_name};

    #[test]
    fn names() {
        assert!(valid_name("Alex"));
        assert!(valid_name("a.b_c-d e"));
        assert!(valid_name("Älex"));
        assert!(!valid_name(""));
        assert!(!valid_name(" lead"));
        assert!(!valid_name("trail "));
        assert!(!valid_name("bad/char"));
        assert!(!valid_name(&"x".repeat(41)));
        assert!(valid_name(&"x".repeat(40)));
        // Multi-byte characters count once, not per byte.
        assert!(valid_name(&"Ä".repeat(40)));
        assert!(!valid_name(&"Ä".repeat(41)));
    }

    #[test]
    fn keys_ignore_case_beyond_ascii() {
        assert_eq!(name_key("Älex"), name_key("älex"));
        assert_eq!(name_key("ÄLEX"), name_key("älex"));
        assert_eq!(name_key("Alex"), name_key("aLEX"));
        assert_eq!(name_key("Élodie"), name_key("éLODIE"));
        assert_eq!(name_key("ΣΊΣΥΦΟΣ"), name_key("σίσυφος"));
        assert_eq!(name_key("Straße"), name_key("STRASSE"));
        assert_eq!(name_key("İstanbul"), name_key("i\u{307}stanbul"));
        assert_ne!(name_key("Alex"), name_key("Alexa"));
        assert_ne!(name_key("Älex"), name_key("Alex"));
    }

    #[test]
    fn ascii_keys_match_the_sql_lower_used_by_the_migration() {
        for n in ["admin", "Riley", "A.b_c-D 9", "ZED"] {
            assert_eq!(name_key(n), n.to_ascii_lowercase());
        }
    }
}
