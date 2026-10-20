//! Configuration from environment variables (all optional).

use std::path::PathBuf;

use crate::auth::TrustedProxy;
use crate::push::PushMode;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub database_path: PathBuf,
    /// Path to a Firebase service-account JSON. Push is disabled when unset.
    pub fcm_service_account: Option<PathBuf>,
    /// Reminders older than this are dropped instead of sent late (server downtime guard).
    pub reminder_grace_mins: i64,
    /// Reverse proxies (IPs or CIDRs) whose `X-Forwarded-For` is believed. Empty = never.
    pub trusted_proxies: Vec<TrustedProxy>,
    /// Soft-deleted rows older than this many days are removed for good.
    pub tombstone_days: i64,
    /// `notification` (default) or `data`; see `push::PushMode`.
    pub push_mode: PushMode,
}

impl Config {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Config {
            listen: get("BREADTRACKER_LISTEN").unwrap_or_else(|| "127.0.0.1:7290".into()),
            database_path: get("BREADTRACKER_DATABASE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("bread-tracker.db")),
            fcm_service_account: get("BREADTRACKER_FCM_SERVICE_ACCOUNT").map(PathBuf::from),
            reminder_grace_mins: get("BREADTRACKER_REMINDER_GRACE_MINS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(360),
            trusted_proxies: get("BREADTRACKER_TRUSTED_PROXIES")
                .map(|v| {
                    v.split(',')
                        .filter(|p| !p.trim().is_empty())
                        .filter_map(|p| {
                            let parsed = TrustedProxy::parse(p);
                            if parsed.is_none() {
                                eprintln!("ignoring invalid BREADTRACKER_TRUSTED_PROXIES entry: {p}");
                            }
                            parsed
                        })
                        .collect()
                })
                .unwrap_or_default(),
            tombstone_days: get("BREADTRACKER_TOMBSTONE_DAYS")
                .and_then(|v| v.parse().ok())
                .filter(|d| *d >= 1)
                .unwrap_or(180),
            push_mode: get("BREADTRACKER_PUSH_MODE")
                .map(|v| {
                    PushMode::parse(&v).unwrap_or_else(|| {
                        eprintln!("ignoring invalid BREADTRACKER_PUSH_MODE {v:?}, using notification");
                        PushMode::Notification
                    })
                })
                .unwrap_or_default(),
        }
    }

    pub fn database_url(&self) -> String {
        format!("sqlite://{}", self.database_path.display())
    }
}
