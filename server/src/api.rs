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

use crate::auth::{self, DeviceId, UserId};
use crate::error::{ApiError, ApiResult};
use crate::model::{SyncRequest, SyncResponse};
use crate::push::{Message, SendOutcome};
use crate::store::{PushError, UserDb};
use crate::{store, xp, AppState};

const MAX_BATCH: usize = 5000;

pub fn router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/sync", get(pull).post(sync))
        .route("/device", put(set_device))
        .route("/stats", get(stats))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/push/test", post(push_test))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

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

async fn pair(State(st): State<AppState>, Json(req): Json<PairReq>) -> ApiResult<Json<PairResp>> {
    if !st.pair_limiter.check() {
        return Err(ApiError::TooManyRequests);
    }
    let name = if req.device_name.trim().is_empty() {
        "Android"
    } else {
        req.device_name.trim()
    };
    let name: String = name.chars().take(80).collect();
    match auth::redeem(&st.db, &req.code, &name).await? {
        Some((device_id, token)) => Ok(Json(PairResp { device_id, token })),
        None => Err(ApiError::Unauthorized),
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
        return Err(ApiError::BadRequest(format!(
            "batch too large (max {MAX_BATCH} rows)"
        )));
    }
    for it in &req.items {
        it.validate().map_err(ApiError::BadRequest)?;
    }
    let today = Utc::now().date_naive();
    for c in &req.completions {
        c.validate(today).map_err(ApiError::BadRequest)?;
    }
    // Completion references are validated against the caller's own items inside the write
    // transaction (tombstoned items count, so an old offline client still syncs after another
    // device deleted the item). An unknown reference, or one that only another user holds,
    // rejects the whole batch with nothing written.
    let user = UserDb::new(&st.db, uid);
    match user.apply_push(&req.items, &req.completions).await {
        Ok(()) => {}
        Err(PushError::UnknownItem) => {
            return Err(ApiError::BadRequest(
                "completion references an unknown item".into(),
            ))
        }
        Err(PushError::Db(e)) => return Err(e.into()),
    }
    Ok(Json(user.pull(req.since).await?))
}

#[derive(Deserialize)]
struct DeviceReq {
    fcm_token: Option<String>,
}

/// Register (or clear) the calling device's FCM token.
///
/// A token identifies one app install, so it belongs to exactly one device across all users:
/// registering a token another device holds takes it over, and only that column of the previous
/// holder is cleared. The caller recheck, the takeover and the assignment run in one write
/// transaction, so a failure anywhere leaves both devices exactly as they were, and two
/// concurrent registrations of the same token end with one holder.
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
    }
    let mut tx = st.db.begin_with("BEGIN IMMEDIATE").await?;
    // The device authenticated a moment ago; it may have signed out or been revoked since.
    // Recheck it still exists and still belongs to this user before touching anyone else.
    let caller: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM devices WHERE id = ? AND user_id = ?")
            .bind(&id)
            .bind(uid)
            .fetch_optional(&mut *tx)
            .await?;
    if caller.is_none() {
        return Err(ApiError::Unauthorized);
    }
    if let Some(t) = &token {
        let taken =
            sqlx::query("UPDATE devices SET fcm_token = NULL WHERE fcm_token = ? AND id != ?")
                .bind(t)
                .bind(&id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if taken > 0 {
            tracing::info!(user = uid, device = %id, taken, "fcm token moved to this device");
        }
    }
    let assigned = sqlx::query("UPDATE devices SET fcm_token = ? WHERE id = ? AND user_id = ?")
        .bind(&token)
        .bind(&id)
        .bind(uid)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if assigned != 1 {
        return Err(ApiError::Unauthorized);
    }
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct StatsQuery {
    date: Option<String>,
}

async fn timezone(st: &AppState) -> ApiResult<Tz> {
    Ok(Tz::from_str(&store::get_setting(&st.db, "timezone").await?)
        .unwrap_or(chrono_tz::Australia::Perth))
}

async fn stats(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Json<xp::Stats>> {
    let today = match q.date {
        Some(d) => NaiveDate::parse_from_str(&d, "%Y-%m-%d")
            .map_err(|_| ApiError::BadRequest("date must be YYYY-MM-DD".into()))?,
        None => Utc::now().with_timezone(&timezone(&st).await?).date_naive(),
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

async fn load_settings(st: &AppState) -> ApiResult<Settings> {
    let timezone = store::get_setting(&st.db, "timezone").await?;
    let tz = Tz::from_str(&timezone).unwrap_or(chrono_tz::Australia::Perth);
    let utc_offset_minutes = Utc::now()
        .with_timezone(&tz)
        .offset()
        .fix()
        .local_minus_utc() as i64
        / 60;
    Ok(Settings {
        utc_offset_minutes,
        timezone,
        quiet_start: store::get_setting(&st.db, "quiet_start").await?,
        quiet_end: store::get_setting(&st.db, "quiet_end").await?,
        reminders_enabled: store::get_setting(&st.db, "reminders_enabled").await? == "1",
    })
}

async fn get_settings(State(st): State<AppState>) -> ApiResult<Json<Settings>> {
    Ok(Json(load_settings(&st).await?))
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
    Json(p): Json<SettingsPatch>,
) -> ApiResult<Json<Settings>> {
    // Validate the complete resulting object before mutating any settings.
    let mut tx = st.db.begin_with("BEGIN IMMEDIATE").await?;
    let current: HashMap<String, String> = sqlx::query_as("SELECT key, value FROM settings")
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
    let cur = Settings {
        timezone: current
            .get("timezone")
            .cloned()
            .unwrap_or_else(|| "Australia/Perth".into()),
        quiet_start: current.get("quiet_start").cloned().unwrap_or_default(),
        quiet_end: current.get("quiet_end").cloned().unwrap_or_default(),
        reminders_enabled: current.get("reminders_enabled").is_none_or(|v| v == "1"),
        // This value is read-only response metadata, not persisted settings.
        utc_offset_minutes: 0,
    };
    let start = p.quiet_start.clone().unwrap_or(cur.quiet_start);
    let end = p.quiet_end.clone().unwrap_or(cur.quiet_end);
    if start.is_empty() != end.is_empty() {
        return Err(ApiError::BadRequest(
            "quiet_start and quiet_end must both be set or both be empty".into(),
        ));
    }
    if !valid_time_or_empty(&start) || !valid_time_or_empty(&end) {
        return Err(ApiError::BadRequest(
            "quiet hours must be HH:MM or empty".into(),
        ));
    }
    if let Some(tz) = &p.timezone {
        Tz::from_str(tz).map_err(|_| ApiError::BadRequest("unknown timezone".into()))?;
    }
    if let Some(tz) = &p.timezone {
        sqlx::query("INSERT INTO settings (key, value) VALUES ('timezone', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(tz).execute(&mut *tx).await?;
    }
    if p.quiet_start.is_some() {
        sqlx::query("INSERT INTO settings (key, value) VALUES ('quiet_start', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(&start).execute(&mut *tx).await?;
    }
    if p.quiet_end.is_some() {
        sqlx::query("INSERT INTO settings (key, value) VALUES ('quiet_end', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(&end).execute(&mut *tx).await?;
    }
    if let Some(on) = p.reminders_enabled {
        sqlx::query("INSERT INTO settings (key, value) VALUES ('reminders_enabled', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(if on { "1" } else { "0" }).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Json(load_settings(&st).await?))
}

/// Send a test notification to the calling device.
async fn push_test(
    State(st): State<AppState>,
    Extension(DeviceId(id)): Extension<DeviceId>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<serde_json::Value>> {
    if !st.push.enabled() {
        return Err(ApiError::BadRequest(
            "push is not configured on the server".into(),
        ));
    }
    let token: Option<String> =
        sqlx::query_scalar("SELECT fcm_token FROM devices WHERE id = ? AND user_id = ?")
            .bind(&id)
            .bind(uid)
            .fetch_one(&st.db)
            .await?;
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Err(ApiError::BadRequest(
            "this device has no fcm token registered".into(),
        ));
    };
    let msg = Message {
        token: token.clone(),
        title: "Bread Tracker".into(),
        body: "Test notification".into(),
        data: HashMap::from([("kind".to_string(), "test".to_string())]),
    };
    match st.push.send(&msg).await? {
        SendOutcome::Sent => Ok(Json(serde_json::json!({ "sent": true }))),
        SendOutcome::Unregistered => {
            // Clear only the token that FCM just rejected. If the device registered a new one
            // while the send was in flight, that newer token stays.
            sqlx::query(
                "UPDATE devices SET fcm_token = NULL WHERE id = ? AND user_id = ? AND fcm_token = ?",
            )
            .bind(&id)
            .bind(uid)
            .bind(&token)
            .execute(&st.db)
            .await?;
            Err(ApiError::BadRequest(
                "fcm token is no longer valid; re-register".into(),
            ))
        }
    }
}
