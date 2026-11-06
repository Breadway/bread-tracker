//! Durable shared public-authentication admission. Hold the writer transaction
//! through validation and redemption so successes never consume failure budget.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use sqlx::{Sqlite, Transaction};

use crate::{db::Db, error::ApiError, store::now_ms, AppState};

const WINDOW_MS: i64 = 600_000;
/// Failed attempts a source may spend in one window before pairing and signup deny it.
pub const MAX_FAILURES: i64 = 5;
/// Largest number of live per-source budgets; at capacity a new source is denied rather than
/// evicting a live one.
pub const MAX_SOURCES: i64 = 1024;

#[derive(Debug, Clone)]
pub struct TrustedProxy {
    address: IpAddr,
    prefix: u8,
}

impl TrustedProxy {
    pub fn parse(value: &str) -> Option<Self> {
        let (address, prefix) = match value.trim().split_once('/') {
            Some((address, prefix)) => (
                address.parse::<IpAddr>().ok()?,
                Some(prefix.parse::<u8>().ok()?),
            ),
            None => (value.trim().parse::<IpAddr>().ok()?, None),
        };
        let bits = if address.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(bits);
        (prefix <= bits).then_some(Self { address, prefix })
    }

    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.address, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                u32::from(network) & mask == u32::from(address) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                u128::from(network) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}

/// Untrusted peers cannot choose their budget with forwarded headers. For a
/// trusted peer, walk the chain from the application outward to its first
/// untrusted hop; malformed chains fall back to the transport peer.
pub fn client_ip(peer: IpAddr, forwarded: Option<&str>, trusted: &[TrustedProxy]) -> IpAddr {
    let is_trusted = |ip| trusted.iter().any(|proxy| proxy.contains(ip));
    if !is_trusted(peer) {
        return peer;
    }
    let Some(forwarded) = forwarded else {
        return peer;
    };
    for hop in forwarded.rsplit(',') {
        match hop.trim().parse::<IpAddr>() {
            Ok(ip) if is_trusted(ip) => continue,
            Ok(ip) => return ip,
            Err(_) => return peer,
        }
    }
    peer
}

pub struct ClientIp(pub IpAddr);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|peer| peer.0.ip())
            .unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let forwarded = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok());
        Ok(Self(client_ip(
            peer,
            forwarded,
            &state.config.trusted_proxies,
        )))
    }
}

pub async fn admit(db: &Db, source: IpAddr) -> Result<Transaction<'static, Sqlite>, ApiError> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM public_auth_limits WHERE window_started_at <= ?")
        .bind(now_ms().saturating_sub(WINDOW_MS))
        .execute(&mut *tx)
        .await?;
    let failures: Option<i64> =
        sqlx::query_scalar("SELECT failures FROM public_auth_limits WHERE source_ip = ?")
            .bind(source.to_string())
            .fetch_optional(&mut *tx)
            .await?;
    let blocked = match failures {
        Some(n) => n >= MAX_FAILURES,
        None => {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM public_auth_limits")
                .fetch_one(&mut *tx)
                .await?
                >= MAX_SOURCES
        }
    };
    if blocked {
        tx.commit().await?;
        return Err(ApiError::TooManyRequests);
    }
    Ok(tx)
}

pub async fn fail(mut tx: Transaction<'_, Sqlite>, source: IpAddr) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO public_auth_limits(source_ip, failures, window_started_at) VALUES (?, 1, ?) ON CONFLICT(source_ip) DO UPDATE SET failures = failures + 1")
        .bind(source.to_string()).bind(now_ms()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarding_requires_trusted_peer_and_stops_at_first_untrusted_hop() {
        let peer = "192.0.2.10".parse().unwrap();
        let trusted = [TrustedProxy::parse("192.0.2.0/24").unwrap()];
        let client: IpAddr = "198.51.100.8".parse().unwrap();
        assert_eq!(
            client_ip(peer, Some("203.0.113.1, 198.51.100.8"), &[]),
            peer
        );
        assert_eq!(
            client_ip(
                peer,
                Some("203.0.113.1, 198.51.100.8, 192.0.2.20"),
                &trusted
            ),
            client
        );
        assert_eq!(client_ip(peer, Some("malformed"), &trusted), peer);
        assert!(TrustedProxy::parse("::/0")
            .unwrap()
            .contains("2001:db8::1".parse().unwrap()));
        assert!(TrustedProxy::parse("192.0.2.1/33").is_none());
    }

    #[tokio::test]
    async fn saturation_rejects_new_sources_without_evicting_existing_budgets() {
        let db = crate::db::connect_memory().await.unwrap();
        for i in 0..MAX_SOURCES {
            sqlx::query("INSERT INTO public_auth_limits VALUES (?, 1, ?)")
                .bind(format!("source-{i}"))
                .bind(now_ms())
                .execute(&db)
                .await
                .unwrap();
        }
        assert!(matches!(
            admit(&db, "203.0.113.1".parse().unwrap()).await,
            Err(ApiError::TooManyRequests)
        ));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM public_auth_limits")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, MAX_SOURCES);
        sqlx::query("UPDATE public_auth_limits SET window_started_at = 0")
            .execute(&db)
            .await
            .unwrap();
        let tx = admit(&db, "203.0.113.1".parse().unwrap()).await.unwrap();
        fail(tx, "203.0.113.1".parse().unwrap()).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM public_auth_limits")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn dropped_validation_transaction_does_not_spend_a_failure() {
        let db = crate::db::connect_memory().await.unwrap();
        let source = "203.0.113.1".parse().unwrap();
        let tx = admit(&db, source).await.unwrap();
        tx.rollback().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM public_auth_limits")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
