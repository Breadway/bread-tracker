//! HTTP routes. Everything except `/healthz` and `/pair` requires a device token.

use std::collections::HashMap;
use std::str::FromStr;

use axum::extract::{Query, State};
use axum::middleware;
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use chrono::{NaiveDate, NaiveTime, Offset, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{self, ClientIp, DeviceId, UserId};
use crate::error::{ApiError, ApiResult};
use crate::model::{SyncRequest, SyncResponse};
use crate::push::{Message, SendOutcome};
use crate::store::{UserDb, UserSettings};
use crate::{xp, AppState};

const MAX_BATCH: usize = 5000;

pub fn router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/sync", get(pull).post(sync))
        .route("/device", put(set_device).delete(sign_out))
        .route("/stats", get(stats))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/push/test", post(push_test))
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/pair", post(pair))
        .merge(authed)
        .layer(RequestBodyLimitLayer::new(2 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[derive(Deserialize)]
struct PairReq {
    code: String,
    #[serde(default)]
    device_name: String,
}

#[derive(Serialize)]
struct PairResp {
    device_id: String,
    token: String,
}

async fn pair(
    State(st): State<AppState>,
    ClientIp(src): ClientIp,
    Json(req): Json<PairReq>,
) -> ApiResult<Json<PairResp>> {
    // Only failed attempts count, and only against the caller's own address.
    if st.pair_limiter.is_blocked(src) {
        return Err(ApiError::TooManyRequests);
    }
    let name = if req.device_name.trim().is_empty() { "Android" } else { req.device_name.trim() };
    let name: String = name.chars().take(80).collect();
    match auth::redeem(&st.db, &req.code, &name).await? {
        Some((device_id, token)) => Ok(Json(PairResp { device_id, token })),
        None => {
            st.pair_limiter.record_failure(src);
            Err(ApiError::Unauthorized)
        }
    }
}

#[derive(Deserialize)]
struct PullQuery {
    #[serde(default)]
    since: i64,
}

async fn pull(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Query(q): Query<PullQuery>,
) -> ApiResult<Json<SyncResponse>> {
    Ok(Json(UserDb::new(&st.db, uid).pull(q.since).await?))
}

/// Push local changes, then return everything changed since `since` (one round trip).
async fn sync(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Json(req): Json<SyncRequest>,
) -> ApiResult<Json<SyncResponse>> {
    if req.items.len() + req.completions.len() > MAX_BATCH {
        return Err(ApiError::BadRequest(format!("batch too large (max {MAX_BATCH} rows)")));
    }
    for it in &req.items {
        it.validate().map_err(ApiError::BadRequest)?;
    }
    let today = Utc::now().date_naive();
    for c in &req.completions {
        c.validate(today).map_err(ApiError::BadRequest)?;
    }
    let user = UserDb::new(&st.db, uid);
    user.apply_push(&req.items, &req.completions).await?;
    Ok(Json(user.pull(req.since).await?))
}

#[derive(Deserialize)]
struct DeviceReq {
    fcm_token: Option<String>,
}

async fn set_device(
    State(st): State<AppState>,
    Extension(DeviceId(id)): Extension<DeviceId>,
    Extension(UserId(uid)): Extension<UserId>,
    Json(req): Json<DeviceReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = req.fcm_token.filter(|t| !t.is_empty());
    if let Some(t) = &token {
        if t.len() > 4096 {
            return Err(ApiError::BadRequest("fcm_token too long".into()));
        }
        // A token identifies one app install, so it belongs to exactly one device across all
        // users: a reinstall, or the same phone signing in as someone else, takes it over.
        // Only the token column of the previous holder is cleared, never the device itself.
        let taken = sqlx::query("UPDATE devices SET fcm_token = NULL WHERE fcm_token = ? AND id != ?")
            .bind(t)
            .bind(&id)
            .execute(&st.db)
            .await?
            .rows_affected();
        if taken > 0 {
            tracing::info!(user = uid, device = %id, taken, "fcm token moved to this device");
        }
    }
    sqlx::query("UPDATE devices SET fcm_token = ? WHERE id = ? AND user_id = ?")
        .bind(&token)
        .bind(&id)
        .bind(uid)
        .execute(&st.db)
        .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Sign out: revoke the calling device. Its token stops working immediately.
async fn sign_out(
    State(st): State<AppState>,
    Extension(DeviceId(id)): Extension<DeviceId>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::devices::revoke_self(&st.db, uid, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct StatsQuery {
    date: Option<String>,
}

async fn stats(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Json<xp::Stats>> {
    let today = match q.date {
        Some(d) => NaiveDate::parse_from_str(&d, "%Y-%m-%d").map_err(|_| ApiError::BadRequest("date must be YYYY-MM-DD".into()))?,
        None => Utc::now().with_timezone(&UserDb::new(&st.db, uid).settings().await?.tz()).date_naive(),
    };
    let user = UserDb::new(&st.db, uid);
    let items = user.all_items().await?;
    let completions = user.all_completions().await?;
    Ok(Json(xp::compute(&items, &completions, today)))
}

#[derive(Serialize, Deserialize)]
struct Settings {
    timezone: String,
    quiet_start: String,
    quiet_end: String,
    reminders_enabled: bool,
    /// The timezone's current offset from UTC in minutes. Read only: lets a client compare
    /// the server's zone with the phone's without carrying a timezone database.
    #[serde(default)]
    utc_offset_minutes: i64,
}

fn settings_response(s: &UserSettings) -> Settings {
    let utc_offset_minutes = Utc::now().with_timezone(&s.tz()).offset().fix().local_minus_utc() as i64 / 60;
    Settings {
        utc_offset_minutes,
        timezone: s.timezone.clone(),
        quiet_start: s.quiet_start.clone(),
        quiet_end: s.quiet_end.clone(),
        reminders_enabled: s.reminders_enabled,
    }
}

async fn get_settings(State(st): State<AppState>, Extension(UserId(uid)): Extension<UserId>) -> ApiResult<Json<Settings>> {
    Ok(Json(settings_response(&UserDb::new(&st.db, uid).settings().await?)))
}

#[derive(Deserialize)]
struct SettingsPatch {
    timezone: Option<String>,
    quiet_start: Option<String>,
    quiet_end: Option<String>,
    reminders_enabled: Option<bool>,
}

fn valid_time_or_empty(t: &str) -> bool {
    t.is_empty() || NaiveTime::parse_from_str(t, "%H:%M").is_ok()
}

async fn put_settings(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Json(p): Json<SettingsPatch>,
) -> ApiResult<Json<Settings>> {
    let user = UserDb::new(&st.db, uid);
    let mut s = user.settings().await?;
    if let Some(tz) = p.timezone {
        Tz::from_str(&tz).map_err(|_| ApiError::BadRequest("unknown timezone".into()))?;
        s.timezone = tz;
    }
    for (key, val, field) in [
        ("quiet_start", p.quiet_start, &mut s.quiet_start),
        ("quiet_end", p.quiet_end, &mut s.quiet_end),
    ] {
        if let Some(v) = val {
            if !valid_time_or_empty(&v) {
                return Err(ApiError::BadRequest(format!("{key} must be HH:MM or empty")));
            }
            *field = v;
        }
    }
    // Quiet hours need both ends or neither; a half-set window used to disable them silently.
    if s.quiet_start.is_empty() != s.quiet_end.is_empty() {
        return Err(ApiError::BadRequest("quiet_start and quiet_end must both be set or both be empty".into()));
    }
    if let Some(on) = p.reminders_enabled {
        s.reminders_enabled = on;
    }
    // Validated as a whole, then written in one statement: a rejected request changes nothing.
    user.save_settings(&s).await?;
    Ok(Json(settings_response(&s)))
}

/// Send a test notification to the calling device.
async fn push_test(
    State(st): State<AppState>,
    Extension(DeviceId(id)): Extension<DeviceId>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<serde_json::Value>> {
    if !st.push.enabled() {
        return Err(ApiError::BadRequest("push is not configured on the server".into()));
    }
    let token: Option<String> = sqlx::query_scalar("SELECT fcm_token FROM devices WHERE id = ? AND user_id = ?")
        .bind(&id)
        .bind(uid)
        .fetch_one(&st.db)
        .await?;
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Err(ApiError::BadRequest("this device has no fcm token registered".into()));
    };
    let msg = Message {
        token,
        title: "Bread Tracker".into(),
        body: "Test notification".into(),
        data: HashMap::from([("kind".to_string(), "test".to_string())]),
    };
    match st.push.send(&msg).await? {
        SendOutcome::Sent => Ok(Json(serde_json::json!({ "sent": true }))),
        SendOutcome::Unregistered => {
            sqlx::query("UPDATE devices SET fcm_token = NULL WHERE id = ? AND user_id = ?")
                .bind(&id)
                .bind(uid)
                .execute(&st.db)
                .await?;
            Err(ApiError::BadRequest("fcm token is no longer valid; re-register".into()))
        }
    }
}
