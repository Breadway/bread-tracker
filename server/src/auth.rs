//! Single-user device auth: a one-time pairing code is exchanged for a long-lived bearer
//! token. Only the SHA-256 of codes and tokens is stored.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

/// Counts failed attempts per source and refuses further attempts from a source that has
/// used up its allowance for the window. Successful attempts are not counted, and one
/// source being blocked never affects another. Shared by `/pair` (and later `/signup`).
///
/// Memory is bounded: at most `MAX_SOURCES` sources are tracked and each keeps at most
/// `max` timestamps (the block threshold, so a source is never tracked past the point it
/// is already blocked). When a new source arrives at capacity, the least recently seen
/// tracked source is evicted. Eviction cost is a single scan of `MAX_SOURCES` entries.
pub struct FailureLimiter {
    max: usize,
    window: Duration,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    /// Monotonic counter stamped on each touch; the smallest stamp is the least recent.
    tick: u64,
    sources: HashMap<IpAddr, Source>,
}

struct Source {
    failures: VecDeque<Instant>,
    last_seen: u64,
}

/// Hard ceiling on distinct sources held in memory at once.
pub const MAX_SOURCES: usize = 1024;

impl FailureLimiter {
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            state: Mutex::new(LimiterState {
                tick: 0,
                sources: HashMap::new(),
            }),
        }
    }

    fn trim(q: &mut VecDeque<Instant>, now: Instant, window: Duration) {
        while q.front().is_some_and(|t| now.duration_since(*t) > window) {
            q.pop_front();
        }
    }

    /// True when `src` has already failed `max` times within the window.
    pub fn is_blocked(&self, src: IpAddr) -> bool {
        let mut st = self.state.lock().unwrap();
        st.tick += 1;
        let tick = st.tick;
        let now = Instant::now();
        let Some(entry) = st.sources.get_mut(&src) else {
            return false;
        };
        entry.last_seen = tick;
        Self::trim(&mut entry.failures, now, self.window);
        if entry.failures.is_empty() {
            st.sources.remove(&src);
            return false;
        }
        entry.failures.len() >= self.max
    }

    pub fn record_failure(&self, src: IpAddr) {
        let mut st = self.state.lock().unwrap();
        st.tick += 1;
        let tick = st.tick;
        let now = Instant::now();
        if !st.sources.contains_key(&src) && st.sources.len() >= MAX_SOURCES {
            // Drop expired sources first; only evict a live one if none are expired.
            let window = self.window;
            st.sources.retain(|_, s| {
                Self::trim(&mut s.failures, now, window);
                !s.failures.is_empty()
            });
            if st.sources.len() >= MAX_SOURCES {
                // Never evict a currently blocked source while an unblocked one exists:
                // otherwise a flood of new addresses could reset an idle block. Within each
                // class the least recently seen goes first.
                let max = self.max;
                let victim = st
                    .sources
                    .iter()
                    .min_by_key(|(_, s)| (s.failures.len() >= max, s.last_seen))
                    .map(|(ip, _)| *ip);
                if let Some(victim) = victim {
                    st.sources.remove(&victim);
                }
            }
        }
        let entry = st.sources.entry(src).or_insert_with(|| Source {
            failures: VecDeque::new(),
            last_seen: tick,
        });
        entry.last_seen = tick;
        Self::trim(&mut entry.failures, now, self.window);
        if entry.failures.len() < self.max {
            entry.failures.push_back(now);
        }
    }

    #[cfg(test)]
    fn tracked_sources(&self) -> usize {
        self.state.lock().unwrap().sources.len()
    }

    #[cfg(test)]
    fn failures_for(&self, src: IpAddr) -> usize {
        self.state
            .lock()
            .unwrap()
            .sources
            .get(&src)
            .map_or(0, |s| s.failures.len())
    }
}

#[cfg(test)]
mod limiter_tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(n))
    }

    #[test]
    fn a_source_is_blocked_after_max_failures_and_others_are_not() {
        let l = FailureLimiter::new(5, Duration::from_secs(600));
        for _ in 0..5 {
            l.record_failure(ip(1));
        }
        assert!(l.is_blocked(ip(1)));
        assert!(!l.is_blocked(ip(2)));
    }

    #[test]
    fn repeated_failures_from_one_source_are_capped_at_the_threshold() {
        let l = FailureLimiter::new(5, Duration::from_secs(600));
        for _ in 0..1000 {
            l.record_failure(ip(1));
        }
        assert_eq!(l.failures_for(ip(1)), 5);
        assert!(l.is_blocked(ip(1)));
    }

    #[test]
    fn unique_source_flood_keeps_memory_at_the_cap() {
        let l = FailureLimiter::new(5, Duration::from_secs(600));
        for n in 1..=(MAX_SOURCES as u32 * 4) {
            l.record_failure(ip(n));
        }
        assert_eq!(l.tracked_sources(), MAX_SOURCES);
    }

    #[test]
    fn flood_of_new_sources_does_not_clear_a_recently_active_blocked_source() {
        let l = FailureLimiter::new(5, Duration::from_secs(600));
        for _ in 0..5 {
            l.record_failure(ip(1));
        }
        // Keep the blocked source warm while many other sources fill the table.
        for n in 2..=(MAX_SOURCES as u32 * 3) {
            assert!(
                l.is_blocked(ip(1)),
                "blocked source was evicted before flood {n}"
            );
            l.record_failure(ip(n));
        }
        assert!(l.is_blocked(ip(1)));
    }

    #[test]
    fn an_idle_blocked_source_survives_a_flood_of_unblocked_sources() {
        let l = FailureLimiter::new(5, Duration::from_secs(600));
        for _ in 0..5 {
            l.record_failure(ip(1));
        }
        // No touches to ip(1) during the flood: it is idle but still inside its window.
        for n in 2..=(MAX_SOURCES as u32 * 3) {
            l.record_failure(ip(n));
        }
        assert!(
            l.is_blocked(ip(1)),
            "idle blocked source was evicted by a new-address flood"
        );
    }

    #[test]
    fn expired_sources_are_reclaimed_before_live_ones_are_evicted() {
        let l = FailureLimiter::new(1, Duration::from_millis(1));
        l.record_failure(ip(1));
        std::thread::sleep(Duration::from_millis(5));
        for n in 2..=(MAX_SOURCES as u32 + 1) {
            l.record_failure(ip(n));
        }
        assert!(l.tracked_sources() <= MAX_SOURCES);
        assert!(!l.is_blocked(ip(1)));
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
