//! Configuration from environment variables (all optional).

use std::path::PathBuf;

use crate::push::PushMode;

#[derive(Debug, Clone)]
pub struct Config {
    /// Only these transport peers may provide a forwarded client address.
    pub trusted_proxies: Vec<crate::public_auth::TrustedProxy>,
    pub listen: String,
    pub database_path: PathBuf,
    /// Path to a Firebase service-account JSON. Push is disabled when unset.
    pub fcm_service_account: Option<PathBuf>,
    /// Reminders older than this are dropped instead of sent late (server downtime guard).
    pub reminder_grace_mins: i64,
    /// `notification` (default) or `data`; see `push::PushMode`.
    pub push_mode: PushMode,
    /// Name for the first (admin) user, who owns all data that existed before multi-user.
    pub admin_name: Option<String>,
    /// How long a new invite code stays valid, in days (`invite create --ttl` overrides it).
    pub invite_ttl_days: i64,
    pub limits: Limits,
}

/// Abuse limits for a server that strangers can sign up on. All are per user except the user
/// cap and the request rate (per device token). Rows include retained tombstones;
/// deleting a row does not reclaim its quota until a safe resync protocol exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_users: i64,
    pub max_items: i64,
    pub max_completions: i64,
    pub max_devices: i64,
    /// Authenticated requests per device token per minute.
    pub requests_per_minute: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_users: 20,
            max_items: 2000,
            max_completions: 50_000,
            max_devices: 10,
            requests_per_minute: 120,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Build the configuration from any key lookup (tests pass a map instead of the process
    /// environment, which is shared between parallel tests).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |k: &str| lookup(k).filter(|v| !v.is_empty());
        Config {
            trusted_proxies: get("BREADTRACKER_TRUSTED_PROXIES")
                .map(|value| {
                    value
                        .split(',')
                        .map(|entry| {
                            crate::public_auth::TrustedProxy::parse(entry).unwrap_or_else(|| {
                                panic!("invalid BREADTRACKER_TRUSTED_PROXIES entry: {entry}")
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            listen: get("BREADTRACKER_LISTEN").unwrap_or_else(|| "127.0.0.1:7290".into()),
            database_path: get("BREADTRACKER_DATABASE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("bread-tracker.db")),
            fcm_service_account: get("BREADTRACKER_FCM_SERVICE_ACCOUNT").map(PathBuf::from),
            reminder_grace_mins: get("BREADTRACKER_REMINDER_GRACE_MINS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(360),
            push_mode: get("BREADTRACKER_PUSH_MODE")
                .map(|v| {
                    PushMode::parse(&v).unwrap_or_else(|| {
                        eprintln!(
                            "ignoring invalid BREADTRACKER_PUSH_MODE {v:?}, using notification"
                        );
                        PushMode::Notification
                    })
                })
                .unwrap_or_default(),
            admin_name: get("BREADTRACKER_ADMIN_NAME"),
            limits: {
                let d = Limits::default();
                let n = |k: &str, default: i64| {
                    get(k)
                        .and_then(|v| v.parse::<i64>().ok())
                        .filter(|v| *v >= 1)
                        .unwrap_or(default)
                };
                Limits {
                    max_users: n("BREADTRACKER_MAX_USERS", d.max_users),
                    max_items: n("BREADTRACKER_MAX_ITEMS_PER_USER", d.max_items),
                    max_completions: n("BREADTRACKER_MAX_COMPLETIONS_PER_USER", d.max_completions),
                    max_devices: n("BREADTRACKER_MAX_DEVICES_PER_USER", d.max_devices),
                    requests_per_minute: n(
                        "BREADTRACKER_REQUESTS_PER_MINUTE",
                        d.requests_per_minute,
                    ),
                }
            },
            invite_ttl_days: get("BREADTRACKER_INVITE_TTL_DAYS")
                .and_then(|v| v.parse().ok())
                .filter(|d| (1..=365).contains(d))
                .unwrap_or(7),
        }
    }

    pub fn database_url(&self) -> String {
        format!("sqlite://{}", self.database_path.display())
    }
}
