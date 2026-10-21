//! Listing and revoking paired devices. Used by the `devices` / `revoke` commands and by
//! the app's sign out. Deleting a device row is the whole revocation: its token hash no
//! longer matches, so every later request gets 401.

use crate::db::Db;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub has_push: bool,
}

pub async fn list(db: &Db) -> Result<Vec<DeviceInfo>, sqlx::Error> {
    sqlx::query_as::<_, DeviceInfo>(
        "SELECT id, name, created_at, last_seen, (fcm_token IS NOT NULL AND fcm_token != '') AS has_push
         FROM devices ORDER BY created_at, id",
    )
    .fetch_all(db)
    .await
}

#[derive(Debug, PartialEq, Eq)]
pub enum Revoked {
    /// Removed; carries the device's name.
    Removed(String),
    NotFound,
    /// The prefix matches more than one device.
    Ambiguous(usize),
}

/// Revoke a device by full id or by a unique prefix of at least 4 characters.
pub async fn revoke(db: &Db, id_or_prefix: &str) -> Result<Revoked, sqlx::Error> {
    let key = id_or_prefix.trim();
    if key.len() < 4 || key.contains(['%', '_']) {
        return Ok(Revoked::NotFound);
    }
    let matches: Vec<(String, String)> = sqlx::query_as("SELECT id, name FROM devices WHERE id = ?1 OR id LIKE ?1 || '%'")
        .bind(key)
        .fetch_all(db)
        .await?;
    let exact = matches.iter().find(|(id, _)| id == key);
    let (id, name) = match (exact, matches.len()) {
        (Some(m), _) => m.clone(),
        (None, 0) => return Ok(Revoked::NotFound),
        (None, 1) => matches[0].clone(),
        (None, n) => return Ok(Revoked::Ambiguous(n)),
    };
    sqlx::query("DELETE FROM devices WHERE id = ?").bind(&id).execute(db).await?;
    Ok(Revoked::Removed(name))
}

/// Revoke exactly this device of this user (sign out). Returns whether a row was removed.
pub async fn revoke_self(db: &Db, user_id: i64, id: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM devices WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(db)
        .await?
        .rows_affected()
        > 0)
}
