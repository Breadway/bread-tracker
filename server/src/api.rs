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
use crate::{invites, store, users};
use crate::{xp, AppState};

const MAX_BATCH: usize = 5000;

pub fn router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/sync", get(pull).post(sync))
        .route("/device", put(set_device).delete(sign_out))
        .route("/pair-code", post(new_pair_code))
        .route("/me", get(me))
        .route("/stats", get(stats))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/push/test", post(push_test))
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/pair", post(pair))
        .route("/signup", post(signup))
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
    /// Which account the new device joined. Added for the app, which must notice when a phone
    /// that held one account's data is paired to another. Older clients ignore the fields.
    user_id: i64,
    name: String,
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
    match auth::redeem_limited(&st.db, &req.code, &name, st.config.limits.max_devices).await? {
        Err(auth::RedeemError::DeviceLimit) => Err(ApiError::LimitReached(format!(
            "this account already has the maximum of {} devices; sign out of one first",
            st.config.limits.max_devices
        ))),
        Err(auth::RedeemError::BadCode) => {
            st.pair_limiter.record_failure(src);
            Err(ApiError::Unauthorized)
        }
        Ok((device_id, token)) => {
            let (user_id, name): (i64, String) = sqlx::query_as(
                "SELECT u.id, u.name FROM devices d JOIN users u ON u.id = d.user_id WHERE d.id = ?",
            )
            .bind(&device_id)
            .fetch_one(&st.db)
            .await?;
            Ok(Json(PairResp { device_id, token, user_id, name }))
        }
    }
}

#[derive(Serialize)]
struct MeResp {
    user_id: i64,
    name: String,
    is_admin: bool,
}

/// Who the caller is. Lets an app that was paired before accounts existed learn its account.
async fn me(State(st): State<AppState>, Extension(UserId(uid)): Extension<UserId>) -> ApiResult<Json<MeResp>> {
    let u = users::find_by_id(&st.db, uid).await?.ok_or(ApiError::Unauthorized)?;
    Ok(Json(MeResp { user_id: u.id, name: u.name, is_admin: u.is_admin }))
}

#[derive(Serialize)]
struct PairCodeResp {
    code: String,
    expires_in_secs: i64,
}

/// Authenticated: a short-lived code that adds another device to the caller's own account.
/// It is bound to the caller's user id, taken from their token, never from the request.
async fn new_pair_code(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<PairCodeResp>> {
    let devices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE user_id = ?").bind(uid).fetch_one(&st.db).await?;
    if devices >= st.config.limits.max_devices {
        return Err(ApiError::LimitReached(format!(
            "this account already has the maximum of {} devices; sign out of one first",
            st.config.limits.max_devices
        )));
    }
    let code = auth::create_pairing_code_limited(&st.db, uid).await?;
    tracing::info!(user = uid, "pairing code issued");
    Ok(Json(PairCodeResp { code, expires_in_secs: auth::pairing_code_ttl_secs() }))
}

#[derive(Deserialize)]
struct SignupReq {
    invite: String,
    name: String,
    #[serde(default)]
    device_name: String,
    /// The phone's IANA timezone (for example `Europe/Berlin`). Optional; an unknown name is
    /// ignored and the account keeps the UTC default, so signup never fails over a timezone.
    #[serde(default)]
    timezone: Option<String>,
}

#[derive(Serialize)]
struct SignupResp {
    user_id: i64,
    name: String,
    device_id: String,
    token: String,
}

/// Public. Create an account from an invite code: user, default settings and first device in
/// one transaction. Never creates an admin. Failed attempts (bad invite) are limited per
/// client address with the same limiter type as `/pair`.
async fn signup(
    State(st): State<AppState>,
    ClientIp(src): ClientIp,
    Json(req): Json<SignupReq>,
) -> ApiResult<Json<SignupResp>> {
    if st.signup_limiter.is_blocked(src) {
        return Err(ApiError::TooManyRequests);
    }
    let name = req.name.trim();
    if !invites::name_ok(name) {
        return Err(ApiError::BadRequest(format!(
            "name must be 1 to {} letters, digits, spaces, '.', '_' or '-'",
            users::MAX_NAME_LEN
        )));
    }
    let device_name = if req.device_name.trim().is_empty() { "Android" } else { req.device_name.trim() };
    let device_name: String = device_name.chars().take(80).collect();
    let timezone = req.timezone.as_deref().map(str::trim).filter(|t| !t.is_empty() && Tz::from_str(t).is_ok());
    match invites::signup(&st.db, &req.invite, name, &device_name, timezone, st.config.limits.max_users).await? {
        Ok(s) => {
            tracing::info!(user = s.user_id, "signed up");
            Ok(Json(SignupResp { user_id: s.user_id, name: name.to_string(), device_id: s.device_id, token: s.token }))
        }
        Err(invites::SignupError::BadInvite) => {
            st.signup_limiter.record_failure(src);
            Err(ApiError::Unauthorized)
        }
        Err(invites::SignupError::NameTaken) => Err(ApiError::Conflict("that name is already taken".into())),
        Err(invites::SignupError::ServerFull) => {
            Err(ApiError::LimitReached("this server is not accepting new accounts right now".into()))
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
    match user.apply_push(&req.items, &req.completions, &st.config.limits).await? {
        store::PushOutcome::Applied { skipped_completions } => {
            if skipped_completions > 0 {
                tracing::warn!(user = uid, skipped_completions, "completions for unknown items were not stored");
            }
        }
        store::PushOutcome::LimitReached(msg) => {
            tracing::warn!(user = uid, "{msg}");
            return Err(ApiError::LimitReached(msg));
        }
    }
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
