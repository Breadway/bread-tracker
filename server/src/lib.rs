//! Bread Tracker server: offline-first sync, device auth, FCM reminders, SQLite.

pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod devices;
pub mod error;
pub mod invites;
pub mod model;
pub mod public_auth;
pub mod push;
pub mod stats;
pub mod store;
pub mod users;
pub mod xp;

use std::sync::Arc;

use config::Config;
use db::Db;
use push::PushSender;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<Config>,
    pub push: Arc<dyn PushSender>,
}

impl AppState {
    pub fn new(db: Db, config: Config, push: Arc<dyn PushSender>) -> Self {
        Self {
            db,
            config: Arc::new(config),
            push,
        }
    }
}
