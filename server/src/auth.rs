//! Single-user device auth: a one-time pairing code is exchanged for a long-lived bearer
//! token. Only the SHA-256 of codes and tokens is stored.

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::error::ApiError;
use crate::store::now_ms;
use crate::AppState;

pub(crate) const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_TTL_MS: i64 = 15 * 60 * 1000;
const LAST_SEEN_INTERVAL_MS: i64 = 60 * 1000;

#[derive(Clone, Debug)]
pub struct DeviceId(pub String);

/// The user who owns the authenticated device. Set by [`require_auth`] from the database,
/// never from anything the client sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserId(pub i64);

/// The exact device and credential that authenticated a request. Handlers that grant
/// long-lived access recheck this inside their own write transaction, because the device can
/// be revoked while the request waits for the writer.
#[derive(Clone, Debug)]
pub struct CallerIdentity {
    pub device_id: String,
    pub token_hash: String,
    pub user_id: i64,
}

pub fn hash(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn random_token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill(&mut b);
    hex::encode(b)
}

pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Create a pairing code valid for 15 minutes for the first (admin) user. Returned in
/// `XXXX-XXXX` form.
pub async fn create_pairing_code(db: &Db) -> Result<String, sqlx::Error> {
    create_pairing_code_for(db, crate::users::ADMIN_ID).await
}

/// Create a pairing code that adds a device to `user_id`'s account.
pub async fn create_pairing_code_for(db: &Db, user_id: i64) -> Result<String, sqlx::Error> {
    // The RNG is not Send, so it must be gone before the first await (handlers run on a
    // multi-threaded runtime).
    let raw: String = {
        let mut rng = rand::thread_rng();
        (0..8)
            .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
            .collect()
    };
    sqlx::query("DELETE FROM pairing_codes WHERE expires_at < ?")
        .bind(now_ms())
        .execute(db)
        .await?;
    sqlx::query("INSERT INTO pairing_codes (code_hash, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(hash(&raw))
        .bind(user_id)
        .bind(now_ms() + CODE_TTL_MS)
        .execute(db)
        .await?;
    Ok(format!("{}-{}", &raw[..4], &raw[4..]))
}

/// Redeem a code (single use). Returns `(device_id, token)` on success.
///
/// The code is consumed and the device created in one `BEGIN IMMEDIATE` transaction: if the
/// device cannot be stored, the code is still there and the same code can be redeemed again.
/// The write lock is taken first, so two concurrent redemptions of one code serialize and
/// exactly one of them finds it.
pub async fn redeem(
    db: &Db,
    code: &str,
    device_name: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let used: Option<i64> = sqlx::query_scalar(
        "DELETE FROM pairing_codes WHERE code_hash = ? AND expires_at > ? RETURNING user_id",
    )
    .bind(hash(&normalize_code(code)))
    .bind(now_ms())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(user_id) = used else {
        tx.commit().await?;
        return Ok(None);
    };
    let device = insert_device(&mut tx, user_id, device_name).await?;
    tx.commit().await?;
    Ok(Some(device))
}

/// Create a device for `user_id` and return `(device_id, token)`. Only the token's hash is stored.
pub async fn insert_device(
    conn: &mut sqlx::SqliteConnection,
    user_id: i64,
    device_name: &str,
) -> Result<(String, String), sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let token = random_token();
    let now = now_ms();
    let disabled: Option<i64> = sqlx::query_scalar("SELECT disabled_at FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await?;
    if disabled.is_some() {
        return Err(sqlx::Error::Protocol(
            "cannot issue a device for a disabled account".into(),
        ));
    }
    sqlx::query("INSERT INTO devices (id, user_id, name, token_hash, created_at, last_seen) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(user_id)
        .bind(device_name)
        .bind(hash(&token))
        .bind(now)
        .bind(now)
        .execute(conn)
        .await?;
    Ok((id, token))
}

/// Number of devices the first-run check looks at: only the admin's. Other users' devices say
/// nothing about whether the owner still needs a pairing code.
pub async fn admin_device_count(db: &Db) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE user_id = ?")
        .bind(crate::users::ADMIN_ID)
        .fetch_one(db)
        .await
}

/// Pairing codes a user may have outstanding at once. Creating one more replaces the oldest,
/// so a stuck client cannot pile them up.
const MAX_OPEN_CODES_PER_USER: i64 = 3;

/// A fresh short-lived pairing code for the caller, keeping at most
/// [`MAX_OPEN_CODES_PER_USER`] unexpired codes for that user.
///
/// Returns `None` without changing anything when the caller is no longer eligible. The check
/// runs inside the same `BEGIN IMMEDIATE` transaction as the prune and insert: authentication
/// happens before the handler, but a request can wait for the writer long enough for its device
/// to be revoked, so the decision to issue access must be made again under the write lock.
pub async fn create_pairing_code_limited(
    db: &Db,
    caller: &CallerIdentity,
) -> Result<Option<String>, sqlx::Error> {
    let raw: String = {
        let mut rng = rand::thread_rng();
        (0..8)
            .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
            .collect()
    };
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let eligible: Option<i64> = sqlx::query_scalar(
        "SELECT d.user_id FROM devices d JOIN users u ON u.id = d.user_id
         WHERE d.id = ? AND d.token_hash = ? AND d.user_id = ? AND u.disabled_at IS NULL",
    )
    .bind(&caller.device_id)
    .bind(&caller.token_hash)
    .bind(caller.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if eligible.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }
    let user_id = caller.user_id;
    sqlx::query(
        "DELETE FROM pairing_codes WHERE user_id = ?1 AND code_hash NOT IN
           (SELECT code_hash FROM pairing_codes WHERE user_id = ?1 AND expires_at > ?2
            ORDER BY expires_at DESC, code_hash DESC LIMIT ?3)",
    )
    .bind(user_id)
    .bind(now_ms())
    .bind(MAX_OPEN_CODES_PER_USER - 1)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO pairing_codes(code_hash, user_id, expires_at) VALUES (?, ?, ?)")
        .bind(hash(&raw))
        .bind(user_id)
        .bind(now_ms() + CODE_TTL_MS)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(format!("{}-{}", &raw[..4], &raw[4..])))
}

/// Expiry of a pairing code, for the response of `POST /pair-code`.
pub fn pairing_code_ttl_secs() -> i64 {
    CODE_TTL_MS / 1000
}

/// Middleware: require `Authorization: Bearer <token>` matching a paired device.
pub async fn require_auth(
    State(st): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthorized)?
        .to_string();
    let token_hash = hash(&token);
    let row: Option<(String, i64, i64, Option<i64>)> = sqlx::query_as(
        "SELECT d.id, d.last_seen, d.user_id, u.disabled_at
         FROM devices d JOIN users u ON u.id = d.user_id WHERE d.token_hash = ?",
    )
    .bind(&token_hash)
    .fetch_optional(&st.db)
    .await?;
    let (id, last_seen, user_id, disabled_at) = row.ok_or(ApiError::Unauthorized)?;
    if disabled_at.is_some() {
        return Err(ApiError::Disabled);
    }
    // Per token, so one noisy device cannot use up the server for everyone else.
    if !allow_request(&st.db, &id, st.config.limits.requests_per_minute).await? {
        let still_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM devices WHERE id = ? AND user_id = ?)")
                .bind(&id)
                .bind(user_id)
                .fetch_one(&st.db)
                .await?;
        if !still_exists {
            return Err(ApiError::Unauthorized);
        }
        tracing::warn!(user = user_id, device = %id, "request rate limit hit");
        return Err(ApiError::TooManyRequests);
    }
    // last_seen is informational, so write it at most once per interval, not per request.
    let now = now_ms();
    if now - last_seen >= LAST_SEEN_INTERVAL_MS {
        sqlx::query(
            "UPDATE devices SET last_seen = ? WHERE id = ? AND user_id = ? AND last_seen < ?",
        )
        .bind(now)
        .bind(&id)
        .bind(user_id)
        .bind(now - LAST_SEEN_INTERVAL_MS)
        .execute(&st.db)
        .await?;
    }
    // The user id goes in the log so abuse can be traced to an account. Never the token, and
    // only the path (no query string).
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    req.extensions_mut().insert(DeviceId(id.clone()));
    req.extensions_mut().insert(UserId(user_id));
    req.extensions_mut().insert(CallerIdentity {
        device_id: id,
        token_hash,
        user_id,
    });
    let resp = next.run(req).await;
    tracing::info!(user = user_id, %method, %path, status = resp.status().as_u16(), "request");
    Ok(resp)
}

/// Fixed one-minute window for the per-device request budget.
const REQUEST_WINDOW_MS: i64 = 60_000;

/// Admit one request for `device_id` and report whether it fit the budget.
///
/// The counter lives in SQLite, so every server process using the same database
/// shares one budget instead of each process allowing the full amount. The
/// upsert runs inside `BEGIN IMMEDIATE`: concurrent processes serialize before
/// reading the count, so the increment cannot lose a race. An expired window is
/// reset in the same statement, which keeps expiry work O(1) per request and
/// leaves at most one row per device. Once the counter reaches `limit` the
/// `WHERE` clause stops updating it, so the stored value saturates instead of
/// growing without bound.
pub async fn allow_request(db: &Db, device_id: &str, limit: i64) -> Result<bool, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let now = now_ms();
    let expired_before = now.saturating_sub(REQUEST_WINDOW_MS);
    let admitted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO authenticated_rate_limits(device_id, requests, window_started_at)
         SELECT ?, 1, ? WHERE EXISTS(SELECT 1 FROM devices WHERE id = ?)
         ON CONFLICT(device_id) DO UPDATE SET
           requests = CASE WHEN authenticated_rate_limits.window_started_at <= ?
                           THEN 1 ELSE authenticated_rate_limits.requests + 1 END,
           window_started_at = CASE WHEN authenticated_rate_limits.window_started_at <= ?
                                    THEN excluded.window_started_at
                                    ELSE authenticated_rate_limits.window_started_at END
         WHERE authenticated_rate_limits.window_started_at <= ?
            OR authenticated_rate_limits.requests < ?
         RETURNING requests",
    )
    .bind(device_id)
    .bind(now)
    .bind(device_id)
    .bind(expired_before)
    .bind(expired_before)
    .bind(expired_before)
    .bind(limit)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(admitted.is_some())
}
