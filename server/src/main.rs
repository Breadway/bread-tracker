//! bread-tracker-server entry point.
//!
//!   bread-tracker-server          run the server
//!   bread-tracker-server pair     print a one-time pairing code (valid 15 minutes)
//!   bread-tracker-server devices  list paired devices
//!   bread-tracker-server revoke <id>   remove a device (id or unique prefix, from `devices`)
//!   bread-tracker-server invite create [--ttl 7d]   print a single-use signup code
//!   bread-tracker-server invite list                list invites and their state
//!   bread-tracker-server invite revoke <id>         cancel an unused invite

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use bread_tracker_server::{api, auth, config::Config, db, devices, invites, push, users, AppState};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "bread_tracker_server=info,tower_http=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env();
    if let Some(parent) = config.database_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).ok();
    }
    let db = db::connect_with_admin(&config.database_url(), config.admin_name.as_deref()).await.context("opening database")?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pair") => {
            println!("{}", auth::create_pairing_code(&db).await?);
            return Ok(());
        }
        Some("devices") => {
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
            match devices::revoke(&db, id).await? {
                devices::Revoked::Removed(name) => println!("Revoked device \"{name}\"."),
                devices::Revoked::NotFound => anyhow::bail!("no device matches {id:?} (use at least 4 characters of an id from `devices`)"),
                devices::Revoked::Ambiguous(n) => anyhow::bail!("{id:?} matches {n} devices; use more of the id"),
            }
            return Ok(());
        }
        Some("invite") => {
            match args.get(1).map(String::as_str) {
                Some("create") => {
                    let ttl_ms = match args.iter().position(|a| a == "--ttl") {
                        Some(i) => {
                            let v = args.get(i + 1).context("--ttl needs a value such as 7d, 48h or 90m")?;
                            invites::parse_ttl(v).with_context(|| format!("invalid --ttl {v:?} (use e.g. 7d, 48h, 90m; 1 minute to 365 days)"))?
                        }
                        None => config.invite_ttl_days * 86_400_000,
                    };
                    let (id, code) = invites::create(&db, users::ADMIN_ID, ttl_ms).await?;
                    println!("{code}");
                    eprintln!("Invite {id}, valid until {}. It can be used once; it cannot be shown again.", fmt_ms(bread_tracker_server::store::now_ms() + ttl_ms));
                }
                Some("list") => {
                    let list = invites::list(&db).await?;
                    if list.is_empty() {
                        println!("No invites.");
                    }
                    for i in list {
                        let state = match i.status {
                            invites::InviteStatus::Active => "active".to_string(),
                            invites::InviteStatus::Used(by) => format!("used by {by}"),
                            invites::InviteStatus::Expired => "expired".to_string(),
                            invites::InviteStatus::Revoked => "revoked".to_string(),
                        };
                        println!("{:<4}  created {}  expires {}  {}", i.id, fmt_ms(i.created_at), fmt_ms(i.expires_at), state);
                    }
                }
                Some("revoke") => {
                    let id: i64 = args.get(2).and_then(|v| v.parse().ok()).context("usage: bread-tracker-server invite revoke <id from `invite list`>")?;
                    match invites::revoke(&db, id).await? {
                        invites::Revoked::Revoked => println!("Invite {id} revoked."),
                        invites::Revoked::NotFound => anyhow::bail!("no invite {id}"),
                        invites::Revoked::NotActive => anyhow::bail!("invite {id} is already used, expired or revoked"),
                    }
                }
                _ => anyhow::bail!("usage: bread-tracker-server invite create [--ttl 7d] | list | revoke <id>"),
            }
            return Ok(());
        }
        Some(other) => anyhow::bail!("unknown command {other:?} (expected pair, devices, revoke or invite)"),
        None => {}
    }

    let sender: Arc<dyn push::PushSender> = match &config.fcm_service_account {
        Some(path) => {
            tracing::info!(mode = ?config.push_mode, "push delivery mode");
            Arc::new(push::Fcm::from_file(path, config.push_mode).context("loading FCM service account")?)
        },
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
    let state = AppState::new(db, config, sender);
    push::spawn_scheduler(state.clone());

    let addr: SocketAddr = listen.parse().context("parsing BREADTRACKER_LISTEN")?;
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, api::router(state).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")
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

fn fmt_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "?".into())
}
