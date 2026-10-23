//! Single-user device auth: a one-time pairing code is exchanged for a long-lived bearer
//! token. Only the SHA-256 of codes and tokens is stored.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
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
    let raw: String = (0..8)
        .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
        .collect();
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
pub async fn redeem(
    db: &Db,
    code: &str,
    device_name: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
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
    sqlx::query(
        "INSERT INTO devices (id, name, token_hash, created_at, last_seen) VALUES (?, ?, ?, ?, ?)",
    )
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
    sqlx::query_scalar("SELECT COUNT(*) FROM devices")
        .fetch_one(db)
        .await
}

/// Durable per-source failed-attempt budgets shared across server processes. Successful
/// pairing attempts do not consume a budget. Storage holds at most 1,024 source rows;
/// at capacity new invalid sources are refused until expired rows are reclaimed.
pub const MAX_SOURCES: i64 = 1024;
pub const FAILURE_WINDOW_MS: i64 = 600_000;
pub const MAX_FAILURES: i64 = 5;

/// Record an invalid attempt transactionally; return true when the source is
/// already blocked. SQLite serializes competing writers across requests/processes.
pub async fn record_pair_failure(db: &Db, source: IpAddr) -> Result<bool, sqlx::Error> {
    let now = now_ms();
    let source = source.to_string();
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let existing: Option<(i64, i64)> = sqlx::query_as(
        "SELECT failures, window_started_at FROM pair_rate_limits WHERE source_ip = ?",
    )
    .bind(&source)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((failures, started)) = existing {
        if now.saturating_sub(started) < FAILURE_WINDOW_MS {
            if failures >= MAX_FAILURES {
                tx.commit().await?;
                return Ok(true);
            }
            sqlx::query("UPDATE pair_rate_limits SET failures = failures + 1, updated_at = ? WHERE source_ip = ?")
                .bind(now).bind(&source).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(false);
        }
        sqlx::query("UPDATE pair_rate_limits SET failures = 1, window_started_at = ?, updated_at = ? WHERE source_ip = ?")
            .bind(now).bind(now).bind(&source).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(false);
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pair_rate_limits")
        .fetch_one(&mut *tx)
        .await?;
    if count >= MAX_SOURCES {
        sqlx::query("DELETE FROM pair_rate_limits WHERE window_started_at <= ?")
            .bind(now.saturating_sub(FAILURE_WINDOW_MS))
            .execute(&mut *tx)
            .await?;
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pair_rate_limits")
            .fetch_one(&mut *tx)
            .await?;
        if remaining < MAX_SOURCES {
            sqlx::query("INSERT INTO pair_rate_limits (source_ip, failures, window_started_at, updated_at) VALUES (?, 1, ?, ?)")
                .bind(&source).bind(now).bind(now).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(false);
        }
        tx.commit().await?;
        return Ok(true);
    }
    sqlx::query("INSERT INTO pair_rate_limits (source_ip, failures, window_started_at, updated_at) VALUES (?, 1, ?, ?)")
        .bind(&source).bind(now).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(false)
}

/// Refuse attempts from a new source when every durable per-source slot is
/// occupied. Existing sources retain their own failure counters. This check is
/// only an early admission fast path; record_pair_failure repeats the cap check
/// transactionally after an invalid redemption to handle concurrent admission.
pub async fn source_capacity_exhausted(db: &Db, source: IpAddr) -> Result<bool, sqlx::Error> {
    let source = source.to_string();
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let known: i64 =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pair_rate_limits WHERE source_ip = ?)")
            .bind(&source)
            .fetch_one(&mut *tx)
            .await?;
    if known != 0 {
        tx.commit().await?;
        return Ok(false);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pair_rate_limits")
        .fetch_one(&mut *tx)
        .await?;
    if count >= MAX_SOURCES {
        sqlx::query("DELETE FROM pair_rate_limits WHERE window_started_at <= ?")
            .bind(now_ms().saturating_sub(FAILURE_WINDOW_MS))
            .execute(&mut *tx)
            .await?;
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pair_rate_limits")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(count >= MAX_SOURCES)
}

pub async fn cleanup_pair_failures(db: &Db) -> Result<u64, sqlx::Error> {
    let cutoff = now_ms().saturating_sub(FAILURE_WINDOW_MS);
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let result = sqlx::query("DELETE FROM pair_rate_limits WHERE window_started_at <= ?")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}

pub async fn pairing_code_exists(db: &Db, code: &str) -> Result<bool, sqlx::Error> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pairing_codes WHERE code_hash = ? AND expires_at > ?)",
    )
    .bind(hash(&normalize_code(code)))
    .bind(now_ms())
    .fetch_one(db)
    .await?;
    Ok(exists != 0)
}

pub async fn pair_failure_count(db: &Db) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM pair_rate_limits")
        .fetch_one(db)
        .await
}

#[cfg(test)]
mod limiter_tests {
    use super::*;
    use crate::db::connect_memory;
    use std::net::Ipv4Addr;

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(n))
    }

    #[tokio::test]
    async fn sqlite_budget_is_per_source_persistent_and_capped() {
        let db = connect_memory().await.unwrap();
        for _ in 0..MAX_FAILURES {
            assert!(!record_pair_failure(&db, ip(1)).await.unwrap());
        }
        assert!(record_pair_failure(&db, ip(1)).await.unwrap());
        assert!(!record_pair_failure(&db, ip(2)).await.unwrap());
        assert_eq!(pair_failure_count(&db).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn simultaneous_same_source_failures_cannot_exceed_budget() {
        let db = connect_memory().await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                record_pair_failure(&db, ip(3)).await.unwrap()
            }));
        }
        let mut allowed = 0;
        for task in tasks {
            if !task.await.unwrap() {
                allowed += 1;
            }
        }
        assert_eq!(allowed, MAX_FAILURES as usize);
        assert_eq!(pair_failure_count(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn expired_budget_is_reset_and_expired_rows_are_cleaned() {
        let db = connect_memory().await.unwrap();
        for _ in 0..MAX_FAILURES {
            assert!(!record_pair_failure(&db, ip(4)).await.unwrap());
        }
        sqlx::query("UPDATE pair_rate_limits SET updated_at = 0, window_started_at = 0")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(cleanup_pair_failures(&db).await.unwrap(), 1);
        assert!(!record_pair_failure(&db, ip(4)).await.unwrap());
        assert_eq!(pair_failure_count(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn populated_pre_migration_database_upgrades_without_changing_existing_data() {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("existing.db");
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::migrate!("../migrations").run(&db).await.unwrap();
        sqlx::query("INSERT INTO items (id, kind, name, points, updated_at, seq) VALUES ('keep', 'chore', 'Existing', 2, 1, 1)")
            .execute(&db).await.unwrap();
        db.close().await;

        let raw = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display())).unwrap(),
            )
            .await
            .unwrap();
        sqlx::migrate!("../migrations").run(&raw).await.unwrap();
        let row: (String, String) = sqlx::query_as("SELECT id, name FROM items WHERE id = 'keep'")
            .fetch_one(&raw)
            .await
            .unwrap();
        assert_eq!(row, ("keep".to_owned(), "Existing".to_owned()));
        assert_eq!(pair_failure_count(&raw).await.unwrap(), 0);
        assert!(!record_pair_failure(&raw, ip(5)).await.unwrap());
        assert_eq!(pair_failure_count(&raw).await.unwrap(), 1);
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
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix as u32)
                };
                u32::from(n) & mask == u32::from(i) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix as u32)
                };
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
    let Some(chain) = forwarded_for else {
        return peer;
    };
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
        let xff = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok());
        Ok(ClientIp(client_ip(peer, xff, &st.config.trusted_proxies)))
    }
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
    let id: Option<String> = sqlx::query_scalar("SELECT id FROM devices WHERE token_hash = ?")
        .bind(hash(&token))
        .fetch_optional(&st.db)
        .await?;
    let id = id.ok_or(ApiError::Unauthorized)?;
    sqlx::query("UPDATE devices SET last_seen = ? WHERE id = ?")
        .bind(now_ms())
        .bind(&id)
        .execute(&st.db)
        .await?;
    req.extensions_mut().insert(DeviceId(id));
    Ok(next.run(req).await)
}
