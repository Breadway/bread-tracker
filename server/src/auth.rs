//! Single-user device auth: a one-time pairing code is exchanged for a long-lived bearer
//! token. Only the SHA-256 of codes and tokens is stored.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
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
        (0..8).map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char).collect()
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
pub async fn redeem(db: &Db, code: &str, device_name: &str) -> Result<Option<(String, String)>, sqlx::Error> {
    let used: Option<i64> = sqlx::query_scalar(
        "DELETE FROM pairing_codes WHERE code_hash = ? AND expires_at > ? RETURNING user_id",
    )
    .bind(hash(&normalize_code(code)))
    .bind(now_ms())
    .fetch_optional(db)
    .await?;
    let Some(user_id) = used else {
        return Ok(None);
    };
    let mut conn = db.acquire().await?;
    Ok(Some(insert_device(&mut conn, user_id, device_name).await?))
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

/// A fresh short-lived pairing code for `user_id`, keeping at most
/// [`MAX_OPEN_CODES_PER_USER`] unexpired codes for that user.
pub async fn create_pairing_code_limited(db: &Db, user_id: i64) -> Result<String, sqlx::Error> {
    sqlx::query(
        "DELETE FROM pairing_codes WHERE user_id = ?1 AND code_hash NOT IN
           (SELECT code_hash FROM pairing_codes WHERE user_id = ?1 AND expires_at > ?2
            ORDER BY expires_at DESC LIMIT ?3)",
    )
    .bind(user_id)
    .bind(now_ms())
    .bind(MAX_OPEN_CODES_PER_USER - 1)
    .execute(db)
    .await?;
    create_pairing_code_for(db, user_id).await
}

/// Expiry of a pairing code, for the response of `POST /pair-code`.
pub fn pairing_code_ttl_secs() -> i64 {
    CODE_TTL_MS / 1000
}

/// Counts failed attempts per source and refuses further attempts from a source that has
/// used up its allowance for the window. Successful attempts are not counted, and one
/// source being blocked never affects another. Shared by `/pair` (and later `/signup`).
pub struct FailureLimiter {
    max: usize,
    window: Duration,
    hits: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

/// Above this many tracked sources, expired entries are swept on the next failure.
const SWEEP_AT: usize = 1024;

impl FailureLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { max, window, hits: Mutex::new(HashMap::new()) }
    }

    fn trim(q: &mut VecDeque<Instant>, now: Instant, window: Duration) {
        while q.front().is_some_and(|t| now.duration_since(*t) > window) {
            q.pop_front();
        }
    }

    /// True when `src` has already failed `max` times within the window.
    pub fn is_blocked(&self, src: IpAddr) -> bool {
        let mut hits = self.hits.lock().unwrap();
        let now = Instant::now();
        let Some(q) = hits.get_mut(&src) else { return false };
        Self::trim(q, now, self.window);
        if q.is_empty() {
            hits.remove(&src);
            return false;
        }
        q.len() >= self.max
    }

    pub fn record_failure(&self, src: IpAddr) {
        let mut hits = self.hits.lock().unwrap();
        let now = Instant::now();
        if hits.len() >= SWEEP_AT {
            let window = self.window;
            hits.retain(|_, q| {
                Self::trim(q, now, window);
                !q.is_empty()
            });
        }
        let q = hits.entry(src).or_default();
        Self::trim(q, now, self.window);
        q.push_back(now);
    }
}

/// An IP or CIDR range allowed to set `X-Forwarded-For` (our reverse proxy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedProxy {
    net: IpAddr,
    prefix: u8,
}

impl TrustedProxy {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p.parse::<u8>().ok()?)),
            None => (s, None),
        };
        let net: IpAddr = addr.parse().ok()?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Self { net, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix as u32) };
                u32::from(n) & mask == u32::from(i) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix as u32) };
                u128::from(n) & mask == u128::from(i) & mask
            }
            _ => false,
        }
    }
}

/// Work out which client a request came from. `X-Forwarded-For` is honoured only when the
/// TCP peer is a trusted proxy; the chain is then walked from the right, skipping other
/// trusted proxies, so a client cannot choose its own key by sending the header itself.
pub fn client_ip(peer: IpAddr, forwarded_for: Option<&str>, trusted: &[TrustedProxy]) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|t| t.contains(ip));
    if !is_trusted(peer) {
        return peer;
    }
    let Some(chain) = forwarded_for else { return peer };
    for hop in chain.rsplit(',') {
        match hop.trim().parse::<IpAddr>() {
            Ok(ip) if is_trusted(ip) => continue,
            Ok(ip) => return ip,
            Err(_) => return peer,
        }
    }
    peer
}

/// Extractor: the client address as seen through the trusted proxy setting. Falls back to
/// the unspecified address when the server was not started with connection info (tests).
#[derive(Clone, Copy, Debug)]
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip())
            .unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let xff = parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
        Ok(ClientIp(client_ip(peer, xff, &st.config.trusted_proxies)))
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
    let row: Option<(String, i64, i64)> =
        sqlx::query_as("SELECT id, last_seen, user_id FROM devices WHERE token_hash = ?")
            .bind(hash(&token))
            .fetch_optional(&st.db)
            .await?;
    let (id, last_seen, user_id) = row.ok_or(ApiError::Unauthorized)?;
    // last_seen is informational, so write it at most once per interval, not per request.
    let now = now_ms();
    if now - last_seen >= LAST_SEEN_INTERVAL_MS {
        sqlx::query("UPDATE devices SET last_seen = ? WHERE id = ? AND user_id = ?")
            .bind(now)
            .bind(&id)
            .bind(user_id)
            .execute(&st.db)
            .await?;
    }
    req.extensions_mut().insert(DeviceId(id));
    req.extensions_mut().insert(UserId(user_id));
    Ok(next.run(req).await)
}
