//! bread-tracker-server entry point.
//!
//!   bread-tracker-server          run the server
//!   bread-tracker-server pair [--user <name>]   print a one-time pairing code (valid 15 minutes)
//!   bread-tracker-server devices [--user <name>]   list paired devices
//!   bread-tracker-server revoke <id>   remove a device (id or unique prefix, from `devices`)
//!   bread-tracker-server user list             list users
//!   bread-tracker-server user remove <name> --yes   delete a user and all their data
//!   bread-tracker-server user export <name>    print everything stored for a user as JSON
//!   bread-tracker-server user disable <name>   refuse a user's devices and stop their reminders, keeping data
//!   bread-tracker-server user enable <name>
//!   bread-tracker-server invite create [--ttl 7d]   print a single-use signup code
//!   bread-tracker-server invite list                list invites and their state
//!   bread-tracker-server invite revoke <id>         cancel an unused invite

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use bread_tracker_server::{
    api, auth, config::Config, db, devices, invites, push, users, AppState,
};
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
    if let Some(parent) = config
        .database_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).ok();
    }
    let db = db::connect_with_admin(&config.database_url(), config.admin_name.as_deref())
        .await
        .context("opening database")?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pair") => {
            let user = resolve_user_flag(&db, &args[1..]).await?;
            println!("{}", auth::create_pairing_code_for(&db, user.id).await?);
            eprintln!(
                "Pairing code for {} (valid 15 minutes, single use).",
                user.name
            );
            return Ok(());
        }
        Some("devices") => {
            let selected = resolve_user_flag(&db, &args[1..]).await?;
            let filter = if args.len() > 1 {
                Some(selected.id)
            } else {
                None
            };
            let list = devices::list(&db, filter).await?;
            if list.is_empty() {
                println!("No devices are paired.");
            }
            for d in list {
                println!(
                    "{}  {:<16}  {:<20}  paired {}  last seen {}  push {}",
                    d.id,
                    d.user,
                    d.name,
                    fmt_ms(d.created_at),
                    fmt_ms(d.last_seen),
                    if d.has_push { "yes" } else { "no" }
                );
            }
            return Ok(());
        }
        Some("device") => {
            match args.get(1).map(String::as_str) {
                Some("list") => {
                    let selected = resolve_user_flag(&db, &args[2..]).await?;
                    let filter = if args.len() > 2 {
                        Some(selected.id)
                    } else {
                        None
                    };
                    for device in devices::list(&db, filter).await? {
                        println!("{}  {}  {}", device.id, device.user, device.name);
                    }
                }
                Some("revoke") => {
                    only_positionals(
                        &args[2..],
                        1,
                        "usage: bread-tracker-server device revoke <id>",
                    )?;
                    match devices::revoke(&db, &args[2]).await? {
                        devices::Revoked::Removed(name) => println!("Revoked device {name:?}."),
                        devices::Revoked::NotFound => anyhow::bail!("no matching device"),
                        devices::Revoked::Ambiguous(_) => anyhow::bail!("ambiguous device prefix"),
                    }
                }
                _ => anyhow::bail!("usage: device list [--user <name>] | device revoke <id>"),
            }
            return Ok(());
        }
        Some("user") => {
            match args.get(1).map(String::as_str) {
                Some("list") => {
                    only_positionals(
                        &args[2..],
                        0,
                        "usage: bread-tracker-server user list",
                    )?;
                    for u in users::list(&db).await? {
                        let devices = devices::list(&db, Some(u.id)).await?.len();
                        println!(
                            "{:<4}  {:<20}  {}  {} device(s)  since {}{}",
                            u.id,
                            u.name,
                            if u.is_admin { "admin" } else { "user " },
                            devices,
                            fmt_ms(u.created_at),
                            if u.disabled_at.is_some() { "  DISABLED" } else { "" }
                        );
                    }
                }
                Some("export") => {
                    only_positionals(
                        &args[2..],
                        1,
                        "usage: bread-tracker-server user export <name>",
                    )?;
                    let name = &args[2];
                    let Some(user) = users::find(&db, name).await? else { anyhow::bail!("no user named {name:?}") };
                    println!("{}", serde_json::to_string_pretty(&users::export(&db, user.id).await?)?);
                }
                Some(which @ ("disable" | "enable")) => {
                    only_positionals(
                        &args[2..],
                        1,
                        "usage: bread-tracker-server user disable|enable <name>",
                    )?;
                    let name = &args[2];
                    let Some(user) = users::find(&db, name).await? else { anyhow::bail!("no user named {name:?}") };
                    if !users::set_disabled(&db, user.id, which == "disable").await? {
                        anyhow::bail!("{} is an admin and cannot be disabled", user.name);
                    }
                    println!("{} {}d.", user.name, which);
                }
                Some("remove") => {
                    let (name, confirmed) = remove_name(&args[2..])?;
                    let Some(user) = users::find(&db, name).await? else {
                        anyhow::bail!("no user named {name:?}")
                    };
                    if !confirmed {
                        anyhow::bail!(
                            "this deletes {} and all of their devices, items, completions and settings, and cannot be undone. Run again with --yes to confirm.",
                            user.name
                        );
                    }
                    match users::remove(&db, user.id).await? {
                        Ok(r) => println!(
                            "Removed {}: {} device(s), {} item(s), {} completion(s).",
                            r.name, r.devices, r.items, r.completions
                        ),
                        Err(users::RemoveError::IsAdmin) => {
                            anyhow::bail!("{} is an admin and cannot be removed", user.name)
                        }
                        Err(users::RemoveError::NotFound) => {
                            anyhow::bail!("no user named {name:?}")
                        }
                    }
                }
                _ => anyhow::bail!("usage: bread-tracker-server user list | export <name> | remove <name> --yes | disable <name> | enable <name>"),
            }
            return Ok(());
        }
        Some("revoke") => {
            only_positionals(
                &args[1..],
                1,
                "usage: bread-tracker-server revoke <device id or prefix>",
            )?;
            let id = &args[1];
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
        Some("invite") => {
            match args.get(1).map(String::as_str) {
                Some("create") => {
                    let usage = "usage: bread-tracker-server invite create [--ttl 7d]";
                    let ttl_ms = match &args[2..] {
                        [] => config.invite_ttl_days * 86_400_000,
                        [flag, value] if flag == "--ttl" => {
                            anyhow::ensure!(
                                !value.starts_with('-'),
                                "--ttl needs a value such as 7d, 48h or 90m; {usage}"
                            );
                            invites::parse_ttl(value).with_context(|| format!("invalid --ttl {value:?} (use e.g. 7d, 48h, 90m; 1 minute to 365 days)"))?
                        }
                        [flag, _] if flag.starts_with('-') => {
                            anyhow::bail!("unknown option {flag:?}; {usage}")
                        }
                        _ => anyhow::bail!("{usage}"),
                    };
                    let (id, code) = invites::create(&db, users::ADMIN_ID, ttl_ms).await?;
                    println!("{code}");
                    eprintln!("Invite {id}, valid until {}. It can be used once; it cannot be shown again.", fmt_ms(bread_tracker_server::store::now_ms() + ttl_ms));
                }
                Some("list") => {
                    only_positionals(&args[2..], 0, "usage: bread-tracker-server invite list")?;
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
                        println!(
                            "{:<4}  created {}  expires {}  {}",
                            i.id,
                            fmt_ms(i.created_at),
                            fmt_ms(i.expires_at),
                            state
                        );
                    }
                }
                Some("revoke") => {
                    only_positionals(
                        &args[2..],
                        1,
                        "usage: bread-tracker-server invite revoke <id from `invite list`>",
                    )?;
                    let id: i64 = args[2].parse().with_context(|| {
                        "usage: bread-tracker-server invite revoke <id from `invite list`>"
                    })?;
                    match invites::revoke(&db, id).await? {
                        invites::Revoked::Revoked => println!("Invite {id} revoked."),
                        invites::Revoked::NotFound => anyhow::bail!("no invite {id}"),
                        invites::Revoked::NotActive => {
                            anyhow::bail!("invite {id} is already used, expired or revoked")
                        }
                    }
                }
                _ => anyhow::bail!(
                    "usage: bread-tracker-server invite create [--ttl 7d] | list | revoke <id>"
                ),
            }
            return Ok(());
        }
        Some(other) => anyhow::bail!(
            "unknown command {other:?} (expected pair, devices, revoke, user or invite)"
        ),
        None => {}
    }

    let sender: Arc<dyn push::PushSender> = match &config.fcm_service_account {
        Some(path) => {
            tracing::info!(mode = ?config.push_mode, "push delivery mode");
            Arc::new(
                push::Fcm::from_file(path, config.push_mode)
                    .context("loading FCM service account")?,
            )
        }
        None => {
            tracing::warn!("BREADTRACKER_FCM_SERVICE_ACCOUNT unset: push reminders are disabled");
            Arc::new(push::NoPush)
        }
    };

    // First run only: the owner (admin) has no device yet. Other users' devices do not count,
    // and no codes are ever printed for other users.
    if auth::admin_device_count(&db).await? == 0 {
        let code = auth::create_pairing_code(&db).await?;
        println!("No device is paired yet. Pairing code (valid 15 minutes): {code}");
    }

    let listen = config.listen.clone();
    let state = AppState::new(db, config, sender);
    push::spawn_scheduler(state.clone());

    let addr: SocketAddr = listen.parse().context("parsing BREADTRACKER_LISTEN")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("server error")
}

/// Destructive and state-changing subcommands take exactly their documented arguments. An
/// unknown label, repeated flag or extra positional is a usage error, never something to
/// silently ignore before a delete.
fn only_positionals(args: &[String], expected: usize, usage: &str) -> anyhow::Result<()> {
    if let Some(flag) = args.iter().find(|a| a.starts_with('-')) {
        anyhow::bail!("unknown option {flag:?}; {usage}");
    }
    anyhow::ensure!(
        args.len() == expected,
        "expected {expected} argument(s), got {}; {usage}",
        args.len()
    );
    Ok(())
}

/// `user remove <name> [--yes]`. The name comes first and may not look like a flag; `--yes`
/// may appear at most once after it. Without it the caller must still see the confirmation
/// prompt and nothing is deleted.
fn remove_name(args: &[String]) -> anyhow::Result<(&str, bool)> {
    const USAGE: &str = "usage: bread-tracker-server user remove <name> --yes";
    let name = args
        .first()
        .filter(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("missing user name; {USAGE}"))?;
    let mut confirmed = false;
    for arg in &args[1..] {
        match arg.as_str() {
            "--yes" => {
                anyhow::ensure!(
                    !confirmed,
                    "option \"--yes\" was given more than once; {USAGE}"
                );
                confirmed = true;
            }
            other if other.starts_with('-') => anyhow::bail!("unknown option {other:?}; {USAGE}"),
            other => anyhow::bail!("unexpected extra argument {other:?}; {USAGE}"),
        }
    }
    Ok((name, confirmed))
}

fn fmt_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "?".into())
}

/// `--user <name>` selects a user; without the flag the admin is meant. A name that does not
/// exist is an error, never a silent fallback to the admin.
async fn resolve_user_flag(
    db: &bread_tracker_server::db::Db,
    args: &[String],
) -> anyhow::Result<users::User> {
    anyhow::ensure!(
        args.is_empty() || (args.len() == 2 && args[0] == "--user" && !args[1].starts_with('-')),
        "expected no arguments or --user <name>; unknown selectors never default to admin"
    );
    let name = match args.iter().position(|a| a == "--user") {
        Some(i) => Some(
            args.get(i + 1)
                .filter(|v| !v.starts_with("--"))
                .context("--user needs a user name")?,
        ),
        None => None,
    };
    match name {
        Some(n) => users::find(db, n)
            .await?
            .with_context(|| format!("no user named {n:?} (see `user list`)")),
        None => users::find_by_id(db, users::ADMIN_ID)
            .await?
            .context("the admin user is missing"),
    }
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
