//! HTTP routes. Everything except `/healthz` and `/pair` requires a device token.

use std::collections::HashMap;
use std::str::FromStr;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::middleware;
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use chrono::{NaiveDate, NaiveTime, Offset, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{self, DeviceId, UserId};
use crate::error::{ApiError, ApiResult};
use crate::model::{Completion, Item, SyncRequest, SyncResponse};
use crate::public_auth::{self, ClientIp};
use crate::push::{Message, SendOutcome};
use crate::store::{PushError, UserDb, UserSettings};
use crate::{family, xp, AppState};
use crate::{invites, users};

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
        .route("/pair-code", post(new_pair_code))
        .route("/me", get(me))
        .route("/export", get(export))
        .route("/account", delete(delete_account))
        .route("/stats", get(stats))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/push/test", post(push_test))
        .route("/family/code", post(family_code))
        .route("/family/links", get(family_links).post(family_create_link))
        .route("/family/links/{id}/accept", post(family_accept))
        .route(
            "/family/links/{id}",
            put(family_update_link).delete(family_unlink),
        )
        .route("/family/children", get(family_children))
        .route(
            "/family/children/{id}/items",
            get(family_child_items).post(family_child_create_item),
        )
        .route(
            "/family/children/{id}/items/{item}",
            put(family_child_update_item).delete(family_child_delete_item),
        )
        .route("/family/children/{id}/stats", get(family_child_stats))
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
    /// Which account the new device joined. Added for the app, which must notice when a phone
    /// that held one account's data is paired to another. Older clients ignore the fields.
    user_id: i64,
    name: String,
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
        "SELECT user_id FROM pairing_codes WHERE code_hash = ? AND expires_at > ?",
    )
    .bind(auth::hash(&auth::normalize_code(&req.code)))
    .bind(crate::store::now_ms())
    .fetch_optional(&mut *tx)
    .await?;
    match owner {
        Some(owner) => {
            let disabled: bool =
                sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM users WHERE id = ?")
                    .bind(owner)
                    .fetch_one(&mut *tx)
                    .await?;
            if disabled {
                return Err(ApiError::Disabled);
            }
            let devices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE user_id = ?")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
            if devices >= st.config.limits.max_devices {
                return Err(ApiError::LimitReached(
                    "device limit reached; sign out of another device first".into(),
                ));
            }
            sqlx::query("DELETE FROM pairing_codes WHERE code_hash = ?")
                .bind(auth::hash(&auth::normalize_code(&req.code)))
                .execute(&mut *tx)
                .await?;
            let (device_id, token) = auth::insert_device(&mut tx, owner, &name).await?;
            let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = ?")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(Json(PairResp {
                device_id,
                token,
                user_id: owner,
                name,
            }))
        }
        None => {
            public_auth::fail(tx, source).await?;
            Err(ApiError::Unauthorized)
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
async fn me(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<MeResp>> {
    let u = users::find_by_id(&st.db, uid)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    Ok(Json(MeResp {
        user_id: u.id,
        name: u.name,
        is_admin: u.is_admin,
    }))
}

/// Everything the server holds for the caller, as JSON they can keep: the account, settings,
/// items and completions (tombstones included, marked by `deleted_at`). Scoped like every other
/// route: only the caller's own rows.
async fn export(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = users::export(&st.db, uid)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    tracing::info!(user = uid, "data export");
    Ok(Json(doc))
}

#[derive(Deserialize)]
struct DeleteAccountReq {
    /// Must equal the account name (ignoring case), so a stray request cannot delete anything.
    confirm: String,
}

/// Delete the caller's account and every row they own, in one transaction, through the same
/// code as the CLI's `user remove`. Admin accounts cannot be removed this way.
async fn delete_account(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Json(req): Json<DeleteAccountReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = users::find_by_id(&st.db, uid)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if users::name_key(req.confirm.trim()) != users::name_key(&u.name) {
        return Err(ApiError::BadRequest(
            "confirmation does not match the account name".into(),
        ));
    }
    match users::remove(&st.db, uid).await? {
        Ok(r) => {
            tracing::info!(user = uid, devices = r.devices, items = r.items, completions = r.completions, "account deleted by its owner");
            Ok(Json(serde_json::json!({ "deleted": true })))
        }
        Err(users::RemoveError::IsAdmin) => Err(ApiError::Forbidden(
            "the admin account cannot be deleted from the app; the server owner can remove it on the server".into(),
        )),
        Err(users::RemoveError::NotFound) => Err(ApiError::Unauthorized),
    }
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
    Extension(caller): Extension<auth::CallerIdentity>,
) -> ApiResult<Json<PairCodeResp>> {
    let uid = caller.user_id;
    let devices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE user_id = ?")
        .bind(uid)
        .fetch_one(&st.db)
        .await?;
    if devices >= st.config.limits.max_devices {
        return Err(ApiError::LimitReached(format!(
            "this account already has the maximum of {} devices; sign out of one first",
            st.config.limits.max_devices
        )));
    }
    let code = auth::create_pairing_code_limited(&st.db, &caller)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    tracing::info!(user = uid, "pairing code issued");
    Ok(Json(PairCodeResp {
        code,
        expires_in_secs: auth::pairing_code_ttl_secs(),
    }))
}

#[derive(Serialize)]
struct FamilyCodeResp {
    code: String,
    expires_in_secs: i64,
}

/// Authenticated: the caller offers a short-lived single-use family code. It is bound to the
/// caller's user id from their token, never from the request, and the same code cannot be used
/// on `/pair`.
async fn family_code(
    State(st): State<AppState>,
    Extension(caller): Extension<auth::CallerIdentity>,
) -> ApiResult<Json<FamilyCodeResp>> {
    let code = family::issue_code(&st.db, &caller)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    tracing::info!(user = caller.user_id, "family code issued");
    Ok(Json(FamilyCodeResp {
        code,
        expires_in_secs: family::code_ttl_secs(),
    }))
}

#[derive(Deserialize)]
struct FamilyRedeemReq {
    code: String,
}

/// Partial update: omit a field to leave it alone. The API rejects an empty body so a typo in
/// the field name cannot look like a successful no-op.
#[derive(Deserialize)]
struct FamilyLinkUpdateReq {
    #[serde(default)]
    default_permission: Option<String>,
    #[serde(default)]
    notify_on_complete: Option<bool>,
}

#[derive(Serialize)]
struct FamilyLinkResp {
    id: i64,
    direction: String,
    status: String,
    permission: String,
    notify_on_complete: bool,
    other_name: String,
    created_at: i64,
    accepted_at: Option<i64>,
    revoked_at: Option<i64>,
    pending_expires_at: Option<i64>,
    pending_expired: bool,
}

impl From<family::LinkInfo> for FamilyLinkResp {
    fn from(link: family::LinkInfo) -> Self {
        let pending_expired = link.pending_expired(crate::store::now_ms());
        Self {
            id: link.id,
            direction: link.direction,
            status: link.status,
            permission: link.permission,
            notify_on_complete: link.notify_on_complete,
            other_name: link.other_name,
            created_at: link.created_at,
            accepted_at: link.accepted_at,
            revoked_at: link.revoked_at,
            pending_expires_at: link.pending_expires_at,
            pending_expired,
        }
    }
}

#[derive(Serialize)]
struct FamilyLinksResp {
    links: Vec<FamilyLinkResp>,
}

async fn family_links(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<FamilyLinksResp>> {
    let links = family::links_for(&st.db, uid)
        .await?
        .into_iter()
        .map(FamilyLinkResp::from)
        .collect();
    Ok(Json(FamilyLinksResp { links }))
}

/// Authenticated parent: change the link's default permission and/or completion alerts.
/// `PUT /family/links/{id}` takes either field, at least one. The child and an unowned id get
/// 404, and a pending or revoked link is a 409. Changing the default also advances the
/// sequence of this parent's items under the child without touching their `updated_at`, so the
/// child's next pull learns the new effective permission without a false edit.
async fn family_update_link(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(id): Path<i64>,
    Json(req): Json<FamilyLinkUpdateReq>,
) -> ApiResult<Json<FamilyLinkResp>> {
    if req.default_permission.is_none() && req.notify_on_complete.is_none() {
        return Err(ApiError::BadRequest(
            "send default_permission or notify_on_complete".into(),
        ));
    }
    match family::update_link(
        &st.db,
        uid,
        id,
        req.default_permission.as_deref(),
        req.notify_on_complete,
    )
    .await?
    {
        family::LinkUpdateOutcome::Updated(link) => {
            tracing::info!(user = uid, link = id, "family link updated");
            Ok(Json(FamilyLinkResp::from(link)))
        }
        family::LinkUpdateOutcome::NotFound => Err(ApiError::NotFound),
        family::LinkUpdateOutcome::NotActive => Err(ApiError::Conflict(
            "only an active family link can be changed".into(),
        )),
        family::LinkUpdateOutcome::InvalidPermission => Err(ApiError::BadRequest(format!(
            "default_permission must be one of {}",
            crate::model::PERMISSIONS.join(", ")
        ))),
    }
}

/// Authenticated: redeem a child's family code into a *pending* link. Both the shared
/// per-source admission and the durable per-code budget apply. A successful redemption spends
/// neither: it deletes its per-code attempt row and leaves the source budget untouched.
async fn family_create_link(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    ClientIp(source): ClientIp,
    body: Result<Json<FamilyRedeemReq>, JsonRejection>,
) -> ApiResult<Json<FamilyLinkResp>> {
    let mut tx = public_auth::admit(&st.db, source).await?;
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => {
            public_auth::fail(tx, source).await?;
            return Err(ApiError::BadRequest("invalid family link request".into()));
        }
    };
    match family::redeem_in(&mut tx, uid, &req.code).await? {
        Ok(link) => {
            tx.commit().await?;
            tracing::info!(user = uid, link = link.id, "family link requested");
            Ok(Json(FamilyLinkResp::from(link)))
        }
        Err(family::RedeemError::BadCode) => {
            public_auth::fail(tx, source).await?;
            Err(ApiError::Unauthorized)
        }
        Err(family::RedeemError::Blocked) => {
            public_auth::fail(tx, source).await?;
            Err(ApiError::TooManyRequests)
        }
        Err(family::RedeemError::SelfLink) => {
            tx.commit().await?;
            Err(ApiError::BadRequest(
                "you cannot link your own account as a parent".into(),
            ))
        }
        Err(family::RedeemError::AlreadyLinked) => {
            tx.commit().await?;
            Err(ApiError::Conflict(
                "you are already actively linked to this child".into(),
            ))
        }
    }
}

/// Only the child of the link may accept it, only while it is pending and unexpired.
async fn family_accept(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    match family::accept(&st.db, uid, id).await? {
        family::AcceptOutcome::Accepted => {
            tracing::info!(user = uid, link = id, "family link accepted");
            Ok(Json(serde_json::json!({ "ok": true, "status": "active" })))
        }
        family::AcceptOutcome::NotFound => Err(ApiError::NotFound),
        family::AcceptOutcome::NotPending => Err(ApiError::Conflict(
            "this link is not pending any more".into(),
        )),
        family::AcceptOutcome::Expired => Err(ApiError::Conflict(
            "this link request has expired; ask the parent to request again".into(),
        )),
    }
}

/// Either side may unlink. Revoking is idempotent; a link the caller is not on is a 404 so an
/// unowned id is never confirmed.
async fn family_unlink(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    match family::revoke(&st.db, uid, id).await? {
        family::Revoked::Revoked => {
            tracing::info!(user = uid, link = id, "family link revoked");
            Ok(Json(serde_json::json!({ "ok": true, "status": "revoked" })))
        }
        family::Revoked::AlreadyRevoked => {
            Ok(Json(serde_json::json!({ "ok": true, "status": "revoked" })))
        }
        family::Revoked::NotFound => Err(ApiError::NotFound),
    }
}

#[derive(Serialize)]
struct FamilyChildResp {
    user_id: i64,
    name: String,
    permission: String,
}

#[derive(Serialize)]
struct FamilyChildrenResp {
    children: Vec<FamilyChildResp>,
}

/// The parent's view of one active child. Only active links appear, so a pending or revoked
/// link never confirms that a child exists.
async fn family_children(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
) -> ApiResult<Json<FamilyChildrenResp>> {
    let children = family::active_children(&st.db, uid)
        .await?
        .into_iter()
        .map(|party| FamilyChildResp {
            user_id: party.user_id,
            name: party.name,
            permission: party.permission,
        })
        .collect();
    Ok(Json(FamilyChildrenResp { children }))
}

fn default_weekdays() -> i64 {
    127
}

fn default_active() -> bool {
    true
}

fn default_points(kind: &str) -> i64 {
    match kind {
        "habit" => 15,
        "todo" => 20,
        _ => 10,
    }
}

#[derive(Deserialize)]
struct FamilyItemCreateReq {
    #[serde(default)]
    id: Option<String>,
    kind: String,
    name: String,
    #[serde(default = "default_weekdays")]
    weekdays: i64,
    #[serde(default)]
    remind_time: Option<String>,
    #[serde(default)]
    points: Option<i64>,
    #[serde(default)]
    due_date: Option<String>,
    #[serde(default)]
    sort: i64,
    #[serde(default = "default_active")]
    active: bool,
}

impl FamilyItemCreateReq {
    fn into_item(self) -> Item {
        let points = self.points.unwrap_or_else(|| default_points(&self.kind));
        Item {
            id: self.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            kind: self.kind,
            name: self.name,
            weekdays: self.weekdays,
            remind_time: self.remind_time,
            points,
            due_date: self.due_date,
            sort: self.sort,
            active: self.active,
            updated_at: 0,
            deleted_at: None,
            created_on: None,
            created_by: None,
            permission_override: None,
            effective_permission: None,
            seq: 0,
        }
    }
}

/// Deserialize a nullable field while keeping "absent" distinct from an explicit null.
fn some_option<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

#[derive(Deserialize)]
struct FamilyItemUpdateReq {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    weekdays: Option<i64>,
    #[serde(default, deserialize_with = "some_option")]
    remind_time: Option<Option<String>>,
    #[serde(default)]
    points: Option<i64>,
    #[serde(default, deserialize_with = "some_option")]
    due_date: Option<Option<String>>,
    #[serde(default)]
    sort: Option<i64>,
    #[serde(default)]
    active: Option<bool>,
}

impl From<FamilyItemUpdateReq> for family::ItemPatch {
    fn from(req: FamilyItemUpdateReq) -> Self {
        Self {
            kind: req.kind,
            name: req.name,
            weekdays: req.weekdays,
            remind_time: req.remind_time,
            points: req.points,
            due_date: req.due_date,
            sort: req.sort,
            active: req.active,
        }
    }
}

#[derive(Serialize)]
struct FamilyProgressResp {
    items: Vec<Item>,
    completions: Vec<Completion>,
    stats: xp::Stats,
}

/// Authenticated parent: create an item on an active child's list. Provenance and the
/// timestamp are server-set, and the id is refused rather than overwriting an existing row.
async fn family_child_create_item(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(child_id): Path<i64>,
    Json(req): Json<FamilyItemCreateReq>,
) -> ApiResult<Json<Item>> {
    match family::create_item(&st.db, uid, child_id, &req.into_item(), &st.config.limits).await? {
        family::ItemWriteOutcome::Created(item) => Ok(Json(item)),
        family::ItemWriteOutcome::NotFound => Err(ApiError::NotFound),
        family::ItemWriteOutcome::Collision => {
            Err(ApiError::Conflict("that item id already exists".into()))
        }
        family::ItemWriteOutcome::Invalid(message) => Err(ApiError::BadRequest(message)),
        family::ItemWriteOutcome::LimitReached(message) => Err(ApiError::LimitReached(message)),
        family::ItemWriteOutcome::Updated(_) | family::ItemWriteOutcome::Deleted => {
            unreachable!("create_item only creates")
        }
    }
}

/// Authenticated parent: edit one item the caller created for an active child. An item the
/// child owns is a 404, exactly like an unknown child id.
async fn family_child_update_item(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path((child_id, item_id)): Path<(i64, String)>,
    Json(req): Json<FamilyItemUpdateReq>,
) -> ApiResult<Json<Item>> {
    let patch = family::ItemPatch::from(req);
    if patch.is_empty() {
        return Err(ApiError::BadRequest(
            "send at least one item field to change".into(),
        ));
    }
    match family::update_item(&st.db, uid, child_id, &item_id, &patch).await? {
        family::ItemWriteOutcome::Updated(item) => Ok(Json(item)),
        family::ItemWriteOutcome::NotFound => Err(ApiError::NotFound),
        family::ItemWriteOutcome::Invalid(message) => Err(ApiError::BadRequest(message)),
        family::ItemWriteOutcome::Created(_)
        | family::ItemWriteOutcome::Deleted
        | family::ItemWriteOutcome::Collision
        | family::ItemWriteOutcome::LimitReached(_) => {
            unreachable!("update_item only updates")
        }
    }
}

/// Authenticated parent: delete (tombstone) one item the caller created for an active child.
async fn family_child_delete_item(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path((child_id, item_id)): Path<(i64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    match family::delete_item(&st.db, uid, child_id, &item_id).await? {
        family::ItemWriteOutcome::Deleted => Ok(Json(serde_json::json!({ "ok": true }))),
        family::ItemWriteOutcome::NotFound => Err(ApiError::NotFound),
        family::ItemWriteOutcome::Created(_)
        | family::ItemWriteOutcome::Updated(_)
        | family::ItemWriteOutcome::Invalid(_)
        | family::ItemWriteOutcome::Collision
        | family::ItemWriteOutcome::LimitReached(_) => {
            unreachable!("delete_item only deletes")
        }
    }
}

/// Authenticated parent: only the items this parent created for an active child, only their
/// completions, and assigned-item XP. The filter runs before the shared rules.
async fn family_child_items(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(child_id): Path<i64>,
) -> ApiResult<Json<FamilyProgressResp>> {
    let progress = family::assigned_progress(&st.db, uid, child_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(FamilyProgressResp {
        items: progress.items,
        completions: progress.completions,
        stats: progress.stats,
    }))
}

/// Authenticated parent: assigned-item XP only, never the child's full statistics.
async fn family_child_stats(
    State(st): State<AppState>,
    Extension(UserId(uid)): Extension<UserId>,
    Path(child_id): Path<i64>,
) -> ApiResult<Json<xp::Stats>> {
    let progress = family::assigned_progress(&st.db, uid, child_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(progress.stats))
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
    match invites::signup_in_limited(
        &mut tx,
        &req.invite,
        name,
        &device_name,
        timezone,
        st.config.limits.max_users,
    )
    .await?
    {
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
        Err(invites::SignupError::ServerFull) => Err(ApiError::LimitReached(
            "this server is not accepting new accounts right now".into(),
        )),
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
    let mut response = UserDb::new(&st.db, uid).pull(q.since).await?;
    response.epoch = Some(st.generation.read().unwrap().clone());
    Ok(Json(response))
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
    let today = Utc::now().date_naive();
    for it in &req.items {
        it.validate(today).map_err(ApiError::BadRequest)?;
    }
    for c in &req.completions {
        c.validate(today).map_err(ApiError::BadRequest)?;
    }
    let user = UserDb::new(&st.db, uid);
    let generation = st.generation.read().unwrap().clone();
    // New clients send an empty sync as preflight. On a true generation mismatch, return a
    // full read-only snapshot in one transaction without accepting writes, so a restore is
    // detected before any push can advance the sequence and mask it. Legacy clients omit the
    // epoch and keep the historical push-then-pull contract.
    if req
        .epoch
        .as_deref()
        .is_some_and(|epoch| epoch != generation)
    {
        let mut full = user.pull(0).await?;
        full.epoch = Some(generation);
        if req.items.is_empty() && req.completions.is_empty() {
            return Ok(Json(full));
        }
        return Err(ApiError::BadRequest(
            "sync generation changed; apply snapshot before retrying uploads".into(),
        ));
    }

    // Completion references are validated against the caller's own items inside the write
    // transaction (tombstoned items count, so an old offline client still syncs after another
    // device deleted the item). An unknown reference, or one that only another user holds,
    // rejects the whole batch with nothing written.
    let refused = match user
        .apply_push_limited(&req.items, &req.completions, &st.config.limits)
        .await
    {
        Ok(refused) => refused,
        Err(PushError::UnknownItem) => {
            return Err(ApiError::BadRequest(
                "completion references an unknown item".into(),
            ))
        }
        Err(PushError::Db(e)) if crate::db::is_busy_database_error(&e) => {
            tracing::warn!(error = %e, "sync transaction stayed busy after retry");
            return Err(ApiError::Unavailable);
        }
        Err(PushError::Db(e)) => return Err(e.into()),
        Err(PushError::LimitReached(message)) => return Err(ApiError::LimitReached(message)),
    };
    let mut response = user.pull(req.since).await?;
    response.epoch = Some(generation);
    response.refused = refused;
    Ok(Json(response))
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
    Ok(Json(user.stats(today).await?))
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
