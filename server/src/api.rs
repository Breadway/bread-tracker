//! HTTP routes. Everything except `/healthz` and `/pair` requires a device token.

use std::collections::HashMap;
use std::str::FromStr;

use axum::extract::rejection::JsonRejection;
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
use crate::public_auth::{self, ClientIp};
use crate::push::{Message, SendOutcome};
use crate::store::{PushError, UserDb, UserSettings};
use crate::{invites, users};
use crate::{xp, AppState};

const MAX_BATCH: usize = 5000;

/// Revoke only the authenticated caller's device within its owner scope.
async fn sign_out(
    State(st): State<AppState>,
    Extension(DeviceId(id)): Extension<DeviceId>,
    Extension(UserId(user_id)): Extension<UserId>,
) -> ApiResult<Json<serde_json::Value>> {
    sqlx::query("DELETE FROM devices WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(&st.db)
        .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub fn router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/sync", get(pull).post(sync))
        .route("/device", put(set_device).delete(sign_out))
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
}

async fn pair(
    State(st): State<AppState>,
    ClientIp(source): ClientIp,
    body: Result<Json<PairReq>, JsonRejection>,
) -> ApiResult<Json<PairResp>> {
    let mut tx = public_auth::admit(&st.db, source).await?;
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => {
            public_auth::fail(tx, source).await?;
            return Err(ApiError::BadRequest("invalid pairing request".into()));
        }
    };
    let name = if req.device_name.trim().is_empty() {
        "Android"
    } else {
        req.device_name.trim()
    };
    let name: String = name.chars().take(80).collect();
    let owner: Option<i64> = sqlx::query_scalar(
        "DELETE FROM pairing_codes WHERE code_hash = ? AND expires_at > ? RETURNING user_id",
    )
    .bind(auth::hash(&auth::normalize_code(&req.code)))
    .bind(crate::store::now_ms())
    .fetch_optional(&mut *tx)
    .await?;
    match owner {
        Some(owner) => {
            let (device_id, token) = auth::insert_device(&mut tx, owner, &name).await?;
            tx.commit().await?;
            Ok(Json(PairResp { device_id, token }))
        }
        None => {
            public_auth::fail(tx, source).await?;
            Err(ApiError::Unauthorized)
        }
    }
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
    body: Result<Json<SignupReq>, JsonRejection>,
) -> ApiResult<Json<SignupResp>> {
    let mut tx = public_auth::admit(&st.db, src).await?;
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => {
            public_auth::fail(tx, src).await?;
            return Err(ApiError::BadRequest("invalid signup request".into()));
        }
    };
    let name = req.name.trim();
    if !invites::name_ok(name) {
        public_auth::fail(tx, src).await?;
        return Err(ApiError::BadRequest(format!(
            "name must be 1 to {} letters, digits, spaces, '.', '_' or '-'",
            users::MAX_NAME_LEN
        )));
    }
    let device_name = if req.device_name.trim().is_empty() {
        "Android"
    } else {
        req.device_name.trim()
    };
    let device_name: String = device_name.chars().take(80).collect();
    let timezone = req
        .timezone
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty() && Tz::from_str(t).is_ok());
    sqlx::query("SAVEPOINT signup_redemption")
        .execute(&mut *tx)
        .await?;
    match invites::signup_in(&mut tx, &req.invite, name, &device_name, timezone).await? {
        Ok(s) => {
            tx.commit().await?;
            tracing::info!(user = s.user_id, "signed up");
            Ok(Json(SignupResp {
                user_id: s.user_id,
                name: name.to_string(),
                device_id: s.device_id,
                token: s.token,
            }))
        }
        Err(invites::SignupError::BadInvite) => {
            sqlx::query("ROLLBACK TO signup_redemption")
                .execute(&mut *tx)
                .await?;
            public_auth::fail(tx, src).await?;
            Err(ApiError::Unauthorized)
        }
        Err(invites::SignupError::NameTaken) => {
            sqlx::query("ROLLBACK TO signup_redemption")
                .execute(&mut *tx)
                .await?;
            public_auth::fail(tx, src).await?;
            Err(ApiError::Conflict("that name is already taken".into()))
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
    // This early check is defence in depth: the `assigned != 1` check below also refuses and rolls
    // the transaction back, so removing it is not observable (an equivalent mutation). It stops the
    // takeover before any row is changed.
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

async fn stats(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Json<xp::Stats>> {
    let today = match q.date {
        Some(d) => NaiveDate::parse_from_str(&d, "%Y-%m-%d")
            .map_err(|_| ApiError::BadRequest("date must be YYYY-MM-DD".into()))?,
        None => Utc::now()
            .with_timezone(&UserDb::new(&st.db, uid).settings().await?.tz())
            .date_naive(),
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
    Settings {
        timezone: s.timezone.clone(),
        quiet_start: s.quiet_start.clone(),
        quiet_end: s.quiet_end.clone(),
        reminders_enabled: s.reminders_enabled,
        utc_offset_minutes: Utc::now()
            .with_timezone(&s.tz())
            .offset()
            .fix()
            .local_minus_utc() as i64
            / 60,
    }
}
async fn get_settings(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<Settings>> {
    Ok(Json(settings_response(
        &UserDb::new(&st.db, uid).settings().await?,
    )))
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
    // Acquire the writer reservation BEFORE reading: disjoint partial patches serialize and
    // merge with the last committed values, rather than replacing each other's stale snapshot.
    let mut tx = st.db.begin_with("BEGIN IMMEDIATE").await?;
    let mut s: UserSettings = sqlx::query_as("SELECT timezone, quiet_start, quiet_end, reminders_enabled FROM user_settings WHERE user_id = ?")
        .bind(uid).fetch_one(&mut *tx).await?;
    if let Some(tz) = p.timezone {
        Tz::from_str(&tz).map_err(|_| ApiError::BadRequest("unknown timezone".into()))?;
        s.timezone = tz;
    }
    if let Some(v) = p.quiet_start {
        s.quiet_start = v;
    }
    if let Some(v) = p.quiet_end {
        s.quiet_end = v;
    }
    if s.quiet_start.is_empty() != s.quiet_end.is_empty() {
        return Err(ApiError::BadRequest(
            "quiet_start and quiet_end must both be set or both be empty".into(),
        ));
    }
    if !valid_time_or_empty(&s.quiet_start) || !valid_time_or_empty(&s.quiet_end) {
        return Err(ApiError::BadRequest(
            "quiet hours must be HH:MM or empty".into(),
        ));
    }
    if let Some(v) = p.reminders_enabled {
        s.reminders_enabled = v;
    }
    sqlx::query("UPDATE user_settings SET timezone = ?, quiet_start = ?, quiet_end = ?, reminders_enabled = ? WHERE user_id = ?")
        .bind(&s.timezone).bind(&s.quiet_start).bind(&s.quiet_end).bind(s.reminders_enabled).bind(uid).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(settings_response(&s)))
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
