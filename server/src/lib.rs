//! Bread Tracker server: offline-first sync, device auth, FCM reminders, SQLite.

pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod devices;
pub mod error;
pub mod invites;
pub mod model;
pub mod push;
pub mod store;
pub mod users;
pub mod xp;

use std::sync::Arc;
use std::time::Duration;

use config::Config;
use db::Db;
use push::PushSender;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<Config>,
    pub push: Arc<dyn PushSender>,
    pub pair_limiter: Arc<auth::FailureLimiter>,
    /// Failed signup attempts (unknown, used, expired or revoked invite), counted per client
    /// address with the same limiter type as `/pair`.
    pub signup_limiter: Arc<auth::FailureLimiter>,
}

impl AppState {
    pub fn new(db: Db, config: Config, push: Arc<dyn PushSender>) -> Self {
        Self {
            db,
            config: Arc::new(config),
            push,
            pair_limiter: Arc::new(auth::FailureLimiter::new(5, Duration::from_secs(600))),
            signup_limiter: Arc::new(auth::FailureLimiter::new(10, Duration::from_secs(600))),
        }
    }
}
