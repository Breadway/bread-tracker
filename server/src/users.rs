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
}

pub async fn list(db: &Db) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, is_admin, created_at FROM users ORDER BY id").fetch_all(db).await
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
