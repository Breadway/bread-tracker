//! Bread Tracker server: offline-first sync, device auth, FCM reminders, SQLite.

pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod model;
pub mod push;
pub mod store;
pub mod xp;

use std::sync::Arc;
use std::time::Duration;

use config::Config;
use db::Db;
use push::PushSender;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub generation: Arc<std::sync::RwLock<String>>,
    pub config: Arc<Config>,
    pub push: Arc<dyn PushSender>,
    pub pair_limiter: Arc<auth::PairLimiter>,
}

impl AppState {
    pub fn new(db: Db, generation: String, config: Config, push: Arc<dyn PushSender>) -> Self {
        Self {
            db,
            generation: Arc::new(std::sync::RwLock::new(generation)),
            config: Arc::new(config),
            push,
            pair_limiter: Arc::new(auth::PairLimiter::new(5, Duration::from_secs(600))),
        }
    }
}
