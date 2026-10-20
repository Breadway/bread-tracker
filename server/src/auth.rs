//! Single-user device auth: a one-time pairing code is exchanged for a long-lived bearer
//! token. Only the SHA-256 of codes and tokens is stored.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_TTL_MS: i64 = 15 * 60 * 1000;
const LAST_SEEN_INTERVAL_MS: i64 = 60 * 1000;

#[derive(Clone, Debug)]
pub struct DeviceId(pub String);

pub fn hash(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn random_token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill(&mut b);
    hex::encode(b)
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Create a pairing code valid for 15 minutes. Returned in `XXXX-XXXX` form.
pub async fn create_pairing_code(db: &Db) -> Result<String, sqlx::Error> {
    let mut rng = rand::thread_rng();
    let raw: String = (0..8).map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char).collect();
    sqlx::query("DELETE FROM pairing_codes WHERE expires_at < ?")
        .bind(now_ms())
        .execute(db)
        .await?;
    sqlx::query("INSERT INTO pairing_codes (code_hash, expires_at) VALUES (?, ?)")
        .bind(hash(&raw))
        .bind(now_ms() + CODE_TTL_MS)
        .execute(db)
        .await?;
    Ok(format!("{}-{}", &raw[..4], &raw[4..]))
}

/// Redeem a code (single use). Returns `(device_id, token)` on success.
pub async fn redeem(db: &Db, code: &str, device_name: &str) -> Result<Option<(String, String)>, sqlx::Error> {
    let used: Option<String> = sqlx::query_scalar(
        "DELETE FROM pairing_codes WHERE code_hash = ? AND expires_at > ? RETURNING code_hash",
    )
    .bind(hash(&normalize_code(code)))
    .bind(now_ms())
    .fetch_optional(db)
    .await?;
    if used.is_none() {
        return Ok(None);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let token = random_token();
    let now = now_ms();
    sqlx::query("INSERT INTO devices (id, name, token_hash, created_at, last_seen) VALUES (?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(device_name)
        .bind(hash(&token))
        .bind(now)
        .bind(now)
        .execute(db)
        .await?;
    Ok(Some((id, token)))
}

pub async fn device_count(db: &Db) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM devices").fetch_one(db).await
}

/// Allows a small number of pairing attempts per window, globally (single-user server).
pub struct PairLimiter {
    max: usize,
    window: Duration,
    hits: Mutex<VecDeque<Instant>>,
}

impl PairLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { max, window, hits: Mutex::new(VecDeque::new()) }
    }

    pub fn check(&self) -> bool {
        let mut hits = self.hits.lock().unwrap();
        let now = Instant::now();
        while hits.front().is_some_and(|t| now.duration_since(*t) > self.window) {
            hits.pop_front();
        }
        if hits.len() >= self.max {
            return false;
        }
        hits.push_back(now);
        true
    }
}

/// Middleware: require `Authorization: Bearer <token>` matching a paired device.
pub async fn require_auth(State(st): State<AppState>, mut req: Request, next: Next) -> Result<Response, ApiError> {
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthorized)?
        .to_string();
    let row: Option<(String, i64)> = sqlx::query_as("SELECT id, last_seen FROM devices WHERE token_hash = ?")
        .bind(hash(&token))
        .fetch_optional(&st.db)
        .await?;
    let (id, last_seen) = row.ok_or(ApiError::Unauthorized)?;
    // last_seen is informational, so write it at most once per interval, not per request.
    let now = now_ms();
    if now - last_seen >= LAST_SEEN_INTERVAL_MS {
        sqlx::query("UPDATE devices SET last_seen = ? WHERE id = ?")
            .bind(now)
            .bind(&id)
            .execute(&st.db)
            .await?;
    }
    req.extensions_mut().insert(DeviceId(id));
    Ok(next.run(req).await)
}
