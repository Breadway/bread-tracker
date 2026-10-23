//! bread-tracker-server entry point.
//!
//!   bread-tracker-server          run the server
//!   bread-tracker-server pair     print a one-time pairing code (valid 15 minutes)
//!   bread-tracker-server mark-restored  rotate generation after restoring a DB

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use bread_tracker_server::{api, auth, config::Config, db, push, AppState};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let marker_completed = Arc::new(AtomicBool::new(false));
    let marker_completed_on_drop = marker_completed.clone();
    let _marker_failure_guard = MarkerFailureGuard(marker_completed_on_drop);
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bread_tracker_server=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env();
    let command = std::env::args().nth(1);
    let restore_failed_path = config.database_path.with_extension("sync-restore-failed");
    if let Some(parent) = config
        .database_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let is_marking_restore = command.as_deref() == Some("mark-restored");
    if !is_marking_restore && restore_failed_path.exists() {
        anyhow::bail!("a prior restore or generation marker failed; verify the database and rerun mark-restored before starting");
    }
    if is_marking_restore && restore_failed_path.exists() {
        anyhow::bail!("a prior restore marker failed; verify or restore the database before attempting another marker");
    }
    if is_marking_restore {
        if !config.database_path.exists() {
            anyhow::bail!(
                "database {} does not exist; restore it before marking generation",
                config.database_path.display()
            );
        }
        // Keep the generation sidecar adjacent to the configured database path. The
        // same path is used by normal startup; rotating any other file has no effect.
        std::fs::create_dir_all(
            config
                .database_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new(".")),
        )?;
        let _generation_lock =
            db::GenerationLock::try_acquire_lifetime_exclusive(&config.database_path)
                .context("server is running; stop it before marking a restored database")?;
        let generation_db = db::connect_existing(&config.database_url())
            .await
            .context("opening database to mark restored generation")?;
        let generation = db::next_generation(&config.database_path)?;
        if let Some(parent) = restore_failed_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut failure_marker = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&restore_failed_path)
            .with_context(|| {
                format!(
                    "restore marker already exists at {}; verify database before retry",
                    restore_failed_path.display()
                )
            })?;
        use std::io::Write;
        writeln!(failure_marker, "restore in progress")?;
        failure_marker.sync_all()?;
        std::fs::File::open(
            restore_failed_path
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )?
        .sync_all()?;
        db::mark_restored(&generation_db, &config.database_path, &generation).await?;
        marker_completed.store(true, Ordering::SeqCst);
        std::fs::remove_file(&restore_failed_path)?;
        std::fs::File::open(
            restore_failed_path
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )?
        .sync_all()?;
        println!("Sync generation rotated. Start the server to resume sync.");
        return Ok(());
    }

    if command.as_deref() == Some("pair") {
        // A serialized SQLite transaction can coexist with a running server.
        let db = db::connect(&config.database_url())
            .await
            .context("opening database for pairing code")?;
        println!("{}", auth::create_pairing_code(&db).await?);
        return Ok(());
    }

    // A server owns the generation exclusively for its listener lifetime. Pair is a
    // short SQLite transaction and does not acquire the generation file lock.
    let startup_lock = db::GenerationLock::try_acquire_lifetime_exclusive(&config.database_path)
        .context("another server or restore operation already owns this database")?;
    let db = db::connect(&config.database_url())
        .await
        .context("opening database")?;
    let generation = db::reconcile_generation_locked(&db, &config.database_path).await?;
    let generation_lock = startup_lock;

    let sender: Arc<dyn push::PushSender> = match &config.fcm_service_account {
        Some(path) => Arc::new(push::Fcm::from_file(path).context("loading FCM service account")?),
        None => {
            tracing::warn!("BREADTRACKER_FCM_SERVICE_ACCOUNT unset: push reminders are disabled");
            Arc::new(push::NoPush)
        }
    };

    if auth::device_count(&db).await? == 0 {
        let code = auth::create_pairing_code(&db).await?;
        println!("No device is paired yet. Pairing code (valid 15 minutes): {code}");
    }

    let listen = config.listen.clone();
    let state = AppState::new(db, generation, config, sender);
    let _generation_lock = generation_lock;
    push::spawn_scheduler(state.clone());

    let addr: SocketAddr = listen.parse().context("parsing BREADTRACKER_LISTEN")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("server error")
}

struct MarkerFailureGuard(Arc<AtomicBool>);

impl Drop for MarkerFailureGuard {
    fn drop(&mut self) {
        if self.0.load(Ordering::SeqCst) {
            return;
        }
        let Ok(database) = std::env::var("BREADTRACKER_DATABASE_PATH") else {
            return;
        };
        if std::env::args().nth(1).as_deref() != Some("mark-restored") {
            return;
        }
        let path = std::path::PathBuf::from(database).with_extension("sync-restore-failed");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(file, "restore marker exited before completion");
            let _ = file.sync_all();
        }
    }
}
