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

/// Redeem a pairing code under the durable per-source attempt budget.
/// The writer transaction serializes admission, validation, and accounting across
/// processes, so a crash rolls back both code use and its paired budget update.
pub async fn pair_with_limit(
    db: &Db,
    source: IpAddr,
    code: &str,
    device_name: &str,
) -> Result<PairAttempt, sqlx::Error> {
    let now = now_ms();
    let source = source.to_string();
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM pair_rate_limits WHERE window_started_at <= ?")
        .bind(now.saturating_sub(FAILURE_WINDOW_MS))
        .execute(&mut *tx)
        .await?;
    let existing: Option<(i64, i64)> = sqlx::query_as(
        "SELECT failures, window_started_at FROM pair_rate_limits WHERE source_ip = ?",
    )
    .bind(&source)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((failures, started)) = existing {
        if now.saturating_sub(started) < FAILURE_WINDOW_MS && failures >= MAX_FAILURES {
            tx.commit().await?;
            return Ok(PairAttempt::Throttled);
        }
    } else {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pair_rate_limits")
            .fetch_one(&mut *tx)
            .await?;
        if count >= MAX_SOURCES {
            tx.commit().await?;
            return Ok(PairAttempt::Throttled);
        }
    }
    sqlx::query("INSERT INTO pair_rate_limits (source_ip, failures, window_started_at, updated_at) VALUES (?, 1, ?, ?) ON CONFLICT(source_ip) DO UPDATE SET failures = CASE WHEN pair_rate_limits.window_started_at <= ? THEN 1 ELSE pair_rate_limits.failures + 1 END, window_started_at = CASE WHEN pair_rate_limits.window_started_at <= ? THEN excluded.window_started_at ELSE pair_rate_limits.window_started_at END, updated_at = excluded.updated_at")
        .bind(&source).bind(now).bind(now).bind(now.saturating_sub(FAILURE_WINDOW_MS)).bind(now.saturating_sub(FAILURE_WINDOW_MS))
        .execute(&mut *tx).await?;

    let used: Option<String> = sqlx::query_scalar(
        "DELETE FROM pairing_codes WHERE code_hash = ? AND expires_at > ? RETURNING code_hash",
    )
    .bind(hash(&normalize_code(code)))
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(_) = used else {
        tx.commit().await?;
        return Ok(PairAttempt::InvalidCode);
    };
    let id = uuid::Uuid::new_v4().to_string();
    let token = random_token();
    sqlx::query(
        "INSERT INTO devices (id, name, token_hash, created_at, last_seen) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(device_name)
    .bind(hash(&token))
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let current: i64 =
        sqlx::query_scalar("SELECT failures FROM pair_rate_limits WHERE source_ip = ?")
            .bind(&source)
            .fetch_one(&mut *tx)
            .await?;
    if current <= 1 {
        sqlx::query("DELETE FROM pair_rate_limits WHERE source_ip = ?")
            .bind(&source)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("UPDATE pair_rate_limits SET failures = failures - 1 WHERE source_ip = ?")
            .bind(&source)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(PairAttempt::Paired(id, token))
}

pub enum PairAttempt {
    Throttled,
    InvalidCode,
    Paired(String, String),
}

pub async fn device_count(db: &Db) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM devices")
        .fetch_one(db)
        .await
}

/// Durable per-source failed-attempt budgets shared across processes.
pub const MAX_SOURCES: i64 = 1024;
pub const FAILURE_WINDOW_MS: i64 = 600_000;
pub const MAX_FAILURES: i64 = 5;

pub async fn cleanup_pair_failures(db: &Db) -> Result<u64, sqlx::Error> {
    let cutoff = now_ms().saturating_sub(FAILURE_WINDOW_MS);
    Ok(
        sqlx::query("DELETE FROM pair_rate_limits WHERE window_started_at <= ?")
            .bind(cutoff)
            .execute(db)
            .await?
            .rows_affected(),
    )
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

    async fn issue_code(db: &Db) -> String {
        create_pairing_code(db).await.unwrap()
    }

    #[tokio::test]
    async fn simultaneous_pair_validations_are_serialized_and_limited() {
        let db = connect_memory().await.unwrap();
        let code = issue_code(&db).await;
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let db = db.clone();
            let code = code.clone();
            let guess = format!("bad-{code}");
            tasks.push(tokio::spawn(async move {
                pair_with_limit(&db, ip(3), &guess, "test").await.unwrap()
            }));
        }
        let mut throttled = 0;
        for task in tasks {
            match task.await.unwrap() {
                PairAttempt::Throttled => throttled += 1,
                PairAttempt::Paired(..) | PairAttempt::InvalidCode => {}
            }
        }
        assert_eq!(throttled, 15);
        assert_eq!(device_count(&db).await.unwrap(), 0);
        assert_eq!(pair_failure_count(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn invalid_codes_charge_budget_and_valid_pair_does_not() {
        let db = connect_memory().await.unwrap();
        for n in 0..MAX_FAILURES {
            let result = pair_with_limit(&db, ip(4), "invalid", "test")
                .await
                .unwrap();
            if n < MAX_FAILURES - 1 {
                assert!(matches!(result, PairAttempt::InvalidCode));
            }
        }
        let code = issue_code(&db).await;
        assert!(matches!(
            pair_with_limit(&db, ip(4), &code, "test").await.unwrap(),
            PairAttempt::Throttled
        ));
        assert_eq!(device_count(&db).await.unwrap(), 0);

        let successful = issue_code(&db).await;
        assert!(matches!(
            pair_with_limit(&db, ip(5), &successful, "test")
                .await
                .unwrap(),
            PairAttempt::Paired(..)
        ));
        let success_budget: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pair_rate_limits WHERE source_ip = ?")
                .bind(ip(5).to_string())
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(success_budget, 0);
        let _ = pair_with_limit(&db, ip(5), "invalid", "test")
            .await
            .unwrap();
        let code = issue_code(&db).await;
        assert!(matches!(
            pair_with_limit(&db, ip(5), &code, "test").await.unwrap(),
            PairAttempt::Paired(..)
        ));
        let sum: i64 =
            sqlx::query_scalar("SELECT failures FROM pair_rate_limits WHERE source_ip = ?")
                .bind(ip(5).to_string())
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(sum, 1);
    }

    #[tokio::test]
    async fn failed_device_insert_rolls_back_code_and_budget_charge() {
        let db = connect_memory().await.unwrap();
        let code = issue_code(&db).await;
        sqlx::query("CREATE TRIGGER reject_test_device BEFORE INSERT ON devices BEGIN SELECT RAISE(ABORT, 'test failure'); END")
            .execute(&db).await.unwrap();
        assert!(pair_with_limit(&db, ip(7), &code, "test").await.is_err());
        sqlx::query("DROP TRIGGER reject_test_device")
            .execute(&db)
            .await
            .unwrap();
        assert!(auth_code_exists(&db, &code).await);
        assert_eq!(pair_failure_count(&db).await.unwrap(), 0);
    }

    async fn auth_code_exists(db: &Db, code: &str) -> bool {
        pairing_code_exists(db, code).await.unwrap()
    }

    #[tokio::test]
    async fn crash_after_committed_bad_attempt_expires_with_window() {
        let db = connect_memory().await.unwrap();
        for _ in 0..MAX_FAILURES {
            let _ = pair_with_limit(&db, ip(6), "wrong", "test").await.unwrap();
        }
        assert!(matches!(
            pair_with_limit(&db, ip(6), "wrong", "test").await.unwrap(),
            PairAttempt::Throttled
        ));
        sqlx::query("UPDATE pair_rate_limits SET window_started_at = 0, updated_at = 0")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(cleanup_pair_failures(&db).await.unwrap(), 1);
        assert!(matches!(
            pair_with_limit(&db, ip(6), "wrong", "test").await.unwrap(),
            PairAttempt::InvalidCode
        ));
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
        let migration_0001 = include_str!("../../migrations/0001_init.sql");
        for statement in migration_0001
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            sqlx::query(statement).execute(&db).await.unwrap();
        }
        sqlx::query("CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)")
            .execute(&db).await.unwrap();
        let migration = sqlx::migrate::Migration::new(
            1,
            std::borrow::Cow::Borrowed("init"),
            sqlx::migrate::MigrationType::Simple,
            std::borrow::Cow::Borrowed(migration_0001),
            false,
        );
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)")
            .bind(migration.version).bind(migration.description.as_ref()).bind(migration.checksum.as_ref())
            .execute(&db).await.unwrap();
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
        assert!(matches!(
            pair_with_limit(&raw, ip(5), "invalid", "test")
                .await
                .unwrap(),
            PairAttempt::InvalidCode
        ));
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
