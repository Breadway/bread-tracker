//! Invite codes and signup. An admin creates a code (CLI); a new user redeems it with
//! `POST /signup`, which creates their account, default settings and first device in one
//! transaction. Only the SHA-256 of a code is stored.

use rand::Rng;
use sqlx::Connection;

use crate::auth::{self, hash, normalize_code, CODE_ALPHABET};
use crate::db::Db;
use crate::store::now_ms;
use crate::users;

const DAY_MS: i64 = 86_400_000;

/// A fresh code in `XXXX-XXXX-XXXX` form (12 characters, about 60 bits).
fn new_code() -> String {
    let mut rng = rand::thread_rng();
    let raw: String = (0..12).map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char).collect();
    format!("{}-{}-{}", &raw[..4], &raw[4..8], &raw[8..])
}

/// Create an invite valid for `ttl_ms`. Returns `(id, code)`; the code cannot be shown again.
pub async fn create(db: &Db, created_by: i64, ttl_ms: i64) -> Result<(i64, String), sqlx::Error> {
    let code = new_code();
    let now = now_ms();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO invites (code_hash, created_by, created_at, expires_at) VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(hash(&normalize_code(&code)))
    .bind(created_by)
    .bind(now)
    .bind(now + ttl_ms)
    .fetch_one(db)
    .await?;
    Ok((id, code))
}

/// Parse a lifetime such as `7d`, `48h` or `90m`; a bare number means days. Between one minute
/// and one year.
pub fn parse_ttl(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit_ms) = match s.chars().last()? {
        'd' => (&s[..s.len() - 1], DAY_MS),
        'h' => (&s[..s.len() - 1], 3_600_000),
        'm' => (&s[..s.len() - 1], 60_000),
        c if c.is_ascii_digit() => (s, DAY_MS),
        _ => return None,
    };
    let ms = num.parse::<i64>().ok()?.checked_mul(unit_ms)?;
    (60_000..=365 * DAY_MS).contains(&ms).then_some(ms)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteStatus {
    Active,
    Used(String),
    Expired,
    Revoked,
}

#[derive(Debug, Clone)]
pub struct InviteInfo {
    pub id: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub status: InviteStatus,
}

#[derive(sqlx::FromRow)]
struct InviteRow {
    id: i64,
    created_at: i64,
    expires_at: i64,
    used_at: Option<i64>,
    used_by: Option<String>,
    revoked_at: Option<i64>,
}

pub async fn list(db: &Db) -> Result<Vec<InviteInfo>, sqlx::Error> {
    let rows: Vec<InviteRow> = sqlx::query_as(
        "SELECT i.id, i.created_at, i.expires_at, i.used_at, u.name AS used_by, i.revoked_at
         FROM invites i LEFT JOIN users u ON u.id = i.used_by ORDER BY i.id",
    )
    .fetch_all(db)
    .await?;
    let now = now_ms();
    Ok(rows
        .into_iter()
        .map(|r| {
            let status = if r.used_at.is_some() {
                InviteStatus::Used(r.used_by.unwrap_or_else(|| "(removed user)".into()))
            } else if r.revoked_at.is_some() {
                InviteStatus::Revoked
            } else if r.expires_at <= now {
                InviteStatus::Expired
            } else {
                InviteStatus::Active
            };
            InviteInfo { id: r.id, created_at: r.created_at, expires_at: r.expires_at, status }
        })
        .collect())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Revoked {
    Revoked,
    NotFound,
    /// Already used, revoked or expired, so there is nothing to revoke.
    NotActive,
}

pub async fn revoke(db: &Db, id: i64) -> Result<Revoked, sqlx::Error> {
    let res = sqlx::query("UPDATE invites SET revoked_at = ? WHERE id = ? AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?")
        .bind(now_ms())
        .bind(id)
        .bind(now_ms())
        .execute(db)
        .await?;
    if res.rows_affected() > 0 {
        return Ok(Revoked::Revoked);
    }
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM invites WHERE id = ?").bind(id).fetch_optional(db).await?;
    Ok(if exists.is_some() { Revoked::NotActive } else { Revoked::NotFound })
}

#[derive(Debug)]
pub struct SignedUp {
    pub user_id: i64,
    pub device_id: String,
    pub token: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SignupError {
    /// Unknown, used, expired or revoked invite. Deliberately one case: the caller must not
    /// learn which codes exist.
    BadInvite,
    NameTaken,
}

/// Redeem `code` and create the user, their settings and their first device atomically.
/// The first statement of the transaction is the claim on the invite, which takes SQLite's
/// write lock, so two concurrent redemptions are serialised and exactly one gets the row.
/// A taken name rolls everything back, leaving the invite unspent. The new user is never an
/// admin. `name` must already be validated by the caller.
pub async fn signup(
    db: &Db,
    code: &str,
    name: &str,
    device_name: &str,
    timezone: Option<&str>,
) -> Result<Result<SignedUp, SignupError>, sqlx::Error> {
    let now = now_ms();
    let mut conn = db.acquire().await?;
    let mut tx = conn.begin().await?;

    let claimed: Option<i64> = sqlx::query_scalar(
        "UPDATE invites SET used_at = ?
         WHERE code_hash = ? AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?
         RETURNING id",
    )
    .bind(now)
    .bind(hash(&normalize_code(code)))
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(invite_id) = claimed else {
        return Ok(Err(SignupError::BadInvite));
    };

    let inserted = sqlx::query_scalar::<_, i64>("INSERT INTO users (name, is_admin, created_at) VALUES (?, 0, ?) RETURNING id")
        .bind(name)
        .bind(now)
        .fetch_one(&mut *tx)
        .await;
    let user_id = match inserted {
        Ok(id) => id,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return Ok(Err(SignupError::NameTaken)),
        Err(e) => return Err(e),
    };
    sqlx::query("UPDATE invites SET used_by = ? WHERE id = ?")
        .bind(user_id)
        .bind(invite_id)
        .execute(&mut *tx)
        .await?;
    if let Some(tz) = timezone {
        // The row exists already (created by the trigger on users).
        sqlx::query("UPDATE user_settings SET timezone = ? WHERE user_id = ?")
            .bind(tz)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    let (device_id, token) = auth::insert_device(&mut tx, user_id, device_name).await?;
    tx.commit().await?;
    Ok(Ok(SignedUp { user_id, device_id, token }))
}

/// Validate a user-chosen name for signup.
pub fn name_ok(name: &str) -> bool {
    users::valid_name(name)
}

#[cfg(test)]
mod tests {
    use super::parse_ttl;

    #[test]
    fn ttl() {
        assert_eq!(parse_ttl("7"), Some(7 * 86_400_000));
        assert_eq!(parse_ttl("7d"), Some(7 * 86_400_000));
        assert_eq!(parse_ttl("48h"), Some(48 * 3_600_000));
        assert_eq!(parse_ttl("90m"), Some(90 * 60_000));
        assert_eq!(parse_ttl("0"), None);
        assert_eq!(parse_ttl("30s"), None);
        assert_eq!(parse_ttl("1m"), Some(60_000));
        assert_eq!(parse_ttl("366d"), None);
        assert_eq!(parse_ttl(""), None);
        assert_eq!(parse_ttl("-3d"), None);
        assert_eq!(parse_ttl("d"), None);
    }
}
