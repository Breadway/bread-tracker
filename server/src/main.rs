//! bread-tracker-server entry point.
//!
//!   bread-tracker-server          run the server
//!   bread-tracker-server pair     print a one-time pairing code (valid 15 minutes)
//!   bread-tracker-server devices  list paired devices
//!   bread-tracker-server revoke <id>   remove a device (id or unique prefix, from `devices`)
//!   bread-tracker-server mark-restored  rotate generation after restoring a DB

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use bread_tracker_server::{api, auth, config::Config, db, devices, push, AppState};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        anyhow::bail!("a prior restore or generation marker failed; verify the database and follow the recovery steps in packaging/DEPLOY.md before starting");
    }
    if is_marking_restore && restore_failed_path.exists() {
        anyhow::bail!("a prior restore marker failed; verify or restore the database and complete the documented recovery before attempting another marker");
    }
    if is_marking_restore {
        if !config.database_path.exists() {
            anyhow::bail!(
                "database {} does not exist; restore it before marking generation",
                config.database_path.display()
            );
        }
        // Acquire exclusive ownership before touching any metadata. A benign refusal because
        // a healthy server still holds the lock must leave the database and its files
        // exactly as they were, so the failure guard is armed only after this succeeds.
        let _generation_lock =
            db::GenerationLock::try_acquire_lifetime_exclusive(&config.database_path)
                .context("server is running; stop it before marking a restored database")?;
        let generation_db = db::connect_existing(&config.database_url())
            .await
            .context("opening database to mark restored generation")?;
        let generation = db::next_generation(&config.database_path)?;

        // Ownership is validated and the mutation is about to begin: from here on, any
        // unexpected exit must leave a sentinel so the next start cannot use half-rotated
        // state. The explicit sentinel below is the durable record; the guard covers panics.
        let marker_completed = Arc::new(AtomicBool::new(false));
        let _marker_failure_guard = MarkerFailureGuard {
            completed: marker_completed.clone(),
            path: restore_failed_path.clone(),
        };
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
        match std::fs::remove_file(&restore_failed_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::File::open(
            restore_failed_path
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )?
        .sync_all()?;
        marker_completed.store(true, Ordering::SeqCst);
        println!("Sync generation rotated. Start the server to resume sync.");
        return Ok(());
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pair") => {
            // A short SQLite transaction can coexist with a running server; pairing must not
            // wait for the serving process to exit.
            let db = db::connect_with_admin(&config.database_url(), config.admin_name.as_deref())
                .await
                .context("opening database")?;
            println!("{}", auth::create_pairing_code(&db).await?);
            return Ok(());
        }
        Some("devices") => {
            let db = db::connect_with_admin(&config.database_url(), config.admin_name.as_deref())
                .await
                .context("opening database")?;
            let list = devices::list(&db).await?;
            if list.is_empty() {
                println!("No devices are paired.");
            }
            for d in list {
                println!(
                    "{}  {:<20}  paired {}  last seen {}  push {}",
                    d.id,
                    d.name,
                    fmt_ms(d.created_at),
                    fmt_ms(d.last_seen),
                    if d.has_push { "yes" } else { "no" }
                );
            }
            return Ok(());
        }
        Some("revoke") => {
            let Some(id) = args.get(1) else {
                anyhow::bail!("usage: bread-tracker-server revoke <device id or prefix>");
            };
            let db = db::connect_with_admin(&config.database_url(), config.admin_name.as_deref())
                .await
                .context("opening database")?;
            match devices::revoke(&db, id).await? {
                devices::Revoked::Removed(name) => println!("Revoked device \"{name}\"."),
                devices::Revoked::NotFound => anyhow::bail!(
                    "no device matches {id:?} (use at least 4 characters of an id from `devices`)"
                ),
                devices::Revoked::Ambiguous(n) => {
                    anyhow::bail!("{id:?} matches {n} devices; use more of the id")
                }
            }
            return Ok(());
        }
        Some(other) => {
            anyhow::bail!(
                "unknown command {other:?} (expected pair, devices, revoke or mark-restored)"
            )
        }
        None => {}
    }

    // A server owns the generation exclusively for its listener lifetime. Pair is a
    // short SQLite transaction and does not acquire the generation file lock. Take the lock
    // before opening or migrating the database so a refused start changes nothing.
    let startup_lock = db::GenerationLock::try_acquire_lifetime_exclusive(&config.database_path)
        .context("another server or restore operation already owns this database")?;
    let db = db::connect_with_admin(&config.database_url(), config.admin_name.as_deref())
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
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")
}

/// Armed only once the marker owns the database exclusively and is about to mutate it. A
/// benign refusal (for example the server still holding the lock) therefore never creates a
/// sentinel that would poison the next healthy start.
struct MarkerFailureGuard {
    completed: Arc<AtomicBool>,
    path: std::path::PathBuf,
}

impl Drop for MarkerFailureGuard {
    fn drop(&mut self) {
        if self.completed.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&self.path)
        {
            use std::io::Write;
            let _ = writeln!(file, "restore marker exited before completion");
            let _ = file.sync_all();
        }
    }
}

fn fmt_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "?".into())
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM (what `docker stop` and systemd send).
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "cannot install SIGTERM handler, only Ctrl-C will stop the server");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received, shutting down"),
        _ = term.recv() => tracing::info!("SIGTERM received, shutting down"),
    }
}
