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
pub async fn rename(db: &Db, id: i64, name: &str) -> Result<bool, NameError> {
    if !valid_name(name) {
        return Err(NameError::Invalid);
    }
    let res =
        sqlx::query("UPDATE users SET name = ?, name_key = ?, name_confirmed = 1 WHERE id = ?")
            .bind(name)
            .bind(name_key(name))
            .bind(id)
            .execute(db)
            .await
            .map_err(map_unique)?;
    Ok(res.rows_affected() > 0)
}

/// Find a user by name, with the same caseless comparison that uniqueness uses.
pub async fn find_by_name(db: &Db, name: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, is_admin, created_at FROM users WHERE name_key = ?")
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

/// Fill `name_key` for users that do not have one yet: the non-ASCII names that existed when
/// migration 0005 ran (SQLite cannot case fold them). Runs at every start and is a no-op when
/// nothing is missing. All rows are updated in one transaction. If two users would share a key
/// (for example `Älex` and `älex` created by code that predates the key) nothing is changed
/// and startup fails naming both, because choosing which user keeps the name is the
/// operator's decision. The remedy is to rename one of them while the server is stopped, with
/// `UPDATE users SET name = '<new name>', name_key = NULL WHERE id = <id>`; the key is filled
/// again at the next start.
pub async fn backfill_name_keys(db: &Db) -> anyhow::Result<usize> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let missing: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM users WHERE name_key IS NULL ORDER BY id")
            .fetch_all(&mut *tx)
            .await?;
    for (id, name) in &missing {
        let key = name_key(name);
        let owner: Option<(i64, String)> =
            sqlx::query_as("SELECT id, name FROM users WHERE name_key = ? AND id != ?")
                .bind(&key)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((other_id, other_name)) = owner {
            anyhow::bail!(
                "users {other_id} ({other_name:?}) and {id} ({name:?}) have the same name ignoring case; \
                 rename one (UPDATE users SET name = '<new name>', name_key = NULL WHERE id = {id}) while the server is stopped, then start again"
            );
        }
        sqlx::query("UPDATE users SET name_key = ? WHERE id = ?")
            .bind(&key)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(missing.len())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
    pub created_at: i64,
}

pub async fn list(db: &Db) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, is_admin, created_at FROM users ORDER BY id")
        .fetch_all(db)
        .await
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
