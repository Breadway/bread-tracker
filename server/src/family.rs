//! Family links: a child offers a short-lived single-use code, a parent redeems it into a
//! pending link, and only the child can accept it. Either side can revoke a link at any time.
//!
//! The child is never linked unilaterally: redemption only creates a `pending` row, and the
//! accept route is the only path to `active`. Nothing reads a link as permission unless its
//! status is `active`, which is the seam later item routes use; a pending or revoked link
//! therefore grants no items and no parent reads.

use std::collections::{BTreeMap, HashMap};

use chrono::{NaiveDate, Utc};
use chrono_tz::Tz;
use rand::Rng;
use sqlx::{Sqlite, SqliteConnection, Transaction};

use crate::auth::{hash, normalize_code, CallerIdentity, CODE_ALPHABET};
use crate::config::Limits;
use crate::db::Db;
use crate::model::{valid_permission, Completion, Item};
use crate::store::{
    apply_item_ledger, first_seen_date, next_seq, now_ms, LedgerItem, COMPLETION_COLS, ITEM_COLS,
};
use crate::xp;

/// Family codes expire after 15 minutes, like pairing codes.
pub const CODE_TTL_MS: i64 = 15 * 60 * 1000;
/// A child may keep at most three unexpired family codes; asking for another replaces the
/// oldest, so a stuck client cannot pile them up.
const MAX_OPEN_CODES_PER_CHILD: i64 = 3;
/// Failed redemptions one code hash may spend before further attempts are refused.
pub const MAX_CODE_FAILURES: i64 = 5;
/// Window for the per-code attempt budget.
pub const CODE_ATTEMPT_WINDOW_MS: i64 = 600_000;
/// Largest number of live per-code budgets. At capacity a new hash cannot start a budget, so a
/// guessing source is refused rather than growing the table without bound.
pub const MAX_CODE_BUDGETS: i64 = 1024;

/// A fresh code in `XXXX-XXXX-XXXX` form (12 characters, about 60 bits).
fn new_code() -> String {
    let mut rng = rand::thread_rng();
    let raw: String = (0..12)
        .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
        .collect();
    format!("{}-{}-{}", &raw[..4], &raw[4..8], &raw[8..])
}

/// Seconds a newly issued code stays valid, for the API response.
pub fn code_ttl_secs() -> i64 {
    CODE_TTL_MS / 1000
}

/// The caller's view of a link. `direction` is `parent` when the caller is the parent and
/// `child` when the caller is the child; `other_name` is the other party's display name.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LinkInfo {
    pub id: i64,
    pub direction: String,
    pub status: String,
    pub permission: String,
    pub notify_on_complete: bool,
    pub other_name: String,
    pub created_at: i64,
    pub accepted_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub pending_expires_at: Option<i64>,
}

impl LinkInfo {
    /// A pending link whose acceptance window has passed. The row stays pending until it is
    /// accepted (which then fails) or revoked; clients show it as expired.
    pub fn pending_expired(&self, now: i64) -> bool {
        self.status == "pending"
            && self
                .pending_expires_at
                .is_some_and(|expires_at| expires_at <= now)
    }
}

/// A user on the other end of an active link, with the permission the link grants.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ActiveParty {
    pub user_id: i64,
    pub name: String,
    pub permission: String,
}

/// A link as the server owner sees it in the CLI.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AdminLinkInfo {
    pub id: i64,
    pub parent_name: String,
    pub child_name: String,
    pub status: String,
    pub permission: String,
    pub created_at: i64,
    pub pending_expires_at: Option<i64>,
}

/// Create a family code for the authenticated child, or `None` when the caller's device or
/// account is no longer eligible. The recheck runs under the same `BEGIN IMMEDIATE` as the
/// insert, because a request can wait for the writer long enough to be revoked.
pub async fn issue_code(db: &Db, caller: &CallerIdentity) -> Result<Option<String>, sqlx::Error> {
    // The RNG is not Send, so it must be gone before the first await.
    let raw = new_code();
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let eligible: Option<i64> = sqlx::query_scalar(
        "SELECT d.user_id FROM devices d JOIN users u ON u.id = d.user_id
         WHERE d.id = ? AND d.token_hash = ? AND d.user_id = ? AND u.disabled_at IS NULL",
    )
    .bind(&caller.device_id)
    .bind(&caller.token_hash)
    .bind(caller.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if eligible.is_none() {
        tx.rollback().await?;
        return Ok(None);
    }
    let user_id = caller.user_id;
    let now = now_ms();
    sqlx::query(
        "DELETE FROM family_codes WHERE expires_at < ? AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?;
    // Keep at most three *open* codes. Used and revoked rows stay as history.
    sqlx::query(
        "DELETE FROM family_codes WHERE child_id = ?1 AND used_at IS NULL AND revoked_at IS NULL
           AND expires_at > ?2 AND code_hash NOT IN
           (SELECT code_hash FROM family_codes
            WHERE child_id = ?1 AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?2
            ORDER BY expires_at DESC, code_hash DESC LIMIT ?3)",
    )
    .bind(user_id)
    .bind(now)
    .bind(MAX_OPEN_CODES_PER_CHILD - 1)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO family_codes (code_hash, child_id, created_at, expires_at) VALUES (?, ?, ?, ?)")
        .bind(hash(&normalize_code(&raw)))
        .bind(user_id)
        .bind(now)
        .bind(now + CODE_TTL_MS)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(raw))
}

#[derive(Debug, PartialEq, Eq)]
pub enum RedeemError {
    /// Unknown, used, revoked or expired code. Deliberately one case: the caller must not
    /// learn which codes exist.
    BadCode,
    /// The caller presented their own code.
    SelfLink,
    /// The pair already has an active link; the code was not consumed.
    AlreadyLinked,
    /// The per-code attempt budget is exhausted.
    Blocked,
}

/// Redeem a family code inside the caller's transaction and create a pending link.
///
/// The caller owns the transaction so a failed redemption can be charged to the shared
/// per-source admission in the same atomic write. The code is claimed first (under the write
/// lock), so two concurrent redemptions of one code serialize and exactly one succeeds. A
/// successful redemption deletes the per-code attempt row, so success never spends a failure.
pub async fn redeem_in(
    tx: &mut Transaction<'_, Sqlite>,
    parent_id: i64,
    code: &str,
) -> Result<Result<LinkInfo, RedeemError>, sqlx::Error> {
    let now = now_ms();
    let code_hash = hash(&normalize_code(code));
    sqlx::query("DELETE FROM family_code_attempts WHERE window_started_at <= ?")
        .bind(now.saturating_sub(CODE_ATTEMPT_WINDOW_MS))
        .execute(&mut **tx)
        .await?;
    let failures: Option<i64> =
        sqlx::query_scalar("SELECT failures FROM family_code_attempts WHERE code_hash = ?")
            .bind(&code_hash)
            .fetch_optional(&mut **tx)
            .await?;
    if failures.is_some_and(|n| n >= MAX_CODE_FAILURES) {
        return Ok(Err(RedeemError::Blocked));
    }

    let owner: Option<i64> = sqlx::query_scalar(
        "SELECT child_id FROM family_codes
         WHERE code_hash = ? AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(&code_hash)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(child_id) = owner else {
        return Ok(Err(record_failure(tx, &code_hash, now).await?));
    };
    if child_id == parent_id {
        return Ok(Err(RedeemError::SelfLink));
    }

    let existing: Option<(i64, String)> =
        sqlx::query_as("SELECT id, status FROM family_links WHERE parent_id = ? AND child_id = ?")
            .bind(parent_id)
            .bind(child_id)
            .fetch_optional(&mut **tx)
            .await?;
    if existing
        .as_ref()
        .is_some_and(|(_, status)| status == "active")
    {
        return Ok(Err(RedeemError::AlreadyLinked));
    }

    let claimed = sqlx::query(
        "UPDATE family_codes SET used_at = ?, used_by = ?
         WHERE code_hash = ? AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(now)
    .bind(parent_id)
    .bind(&code_hash)
    .bind(now)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if claimed != 1 {
        return Ok(Err(record_failure(tx, &code_hash, now).await?));
    }

    let pending_expires_at = now + CODE_TTL_MS;
    let link_id = match existing {
        Some((id, _)) => {
            // Re-link after a revoke reuses the unique pair row. It starts a fresh pending
            // request with the defaults, so an old permission or alert setting cannot carry
            // over without the child accepting again.
            sqlx::query(
                "UPDATE family_links SET status = 'pending', default_permission = 'complete_only',
                   notify_on_complete = 0, created_at = ?, accepted_at = NULL, revoked_at = NULL,
                   pending_expires_at = ?
                 WHERE id = ?",
            )
            .bind(now)
            .bind(pending_expires_at)
            .bind(id)
            .execute(&mut **tx)
            .await?;
            id
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO family_links (parent_id, child_id, status, default_permission,
               notify_on_complete, created_at, pending_expires_at)
             VALUES (?, ?, 'pending', 'complete_only', 0, ?, ?) RETURNING id",
            )
            .bind(parent_id)
            .bind(child_id)
            .bind(now)
            .bind(pending_expires_at)
            .fetch_one(&mut **tx)
            .await?
        }
    };
    sqlx::query("DELETE FROM family_code_attempts WHERE code_hash = ?")
        .bind(&code_hash)
        .execute(&mut **tx)
        .await?;
    let link = link_info(tx, link_id, parent_id).await?;
    Ok(Ok(link))
}

/// Redeem a code in its own transaction. The API adds the shared per-source admission around
/// this; tests and other callers get the same single-code behavior.
pub async fn redeem(
    db: &Db,
    parent_id: i64,
    code: &str,
) -> Result<Result<LinkInfo, RedeemError>, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let result = redeem_in(&mut tx, parent_id, code).await?;
    tx.commit().await?;
    Ok(result)
}

/// Record one failed attempt for a code hash. Returns `BadCode` when it fitted the bounded
/// table, or `Blocked` when the table is at capacity and this hash had no budget yet.
async fn record_failure(
    tx: &mut Transaction<'_, Sqlite>,
    code_hash: &str,
    now: i64,
) -> Result<RedeemError, sqlx::Error> {
    let failures: Option<i64> =
        sqlx::query_scalar("SELECT failures FROM family_code_attempts WHERE code_hash = ?")
            .bind(code_hash)
            .fetch_optional(&mut **tx)
            .await?;
    if failures.is_some() {
        sqlx::query(
            "UPDATE family_code_attempts SET failures = failures + 1, updated_at = ? WHERE code_hash = ?",
        )
        .bind(now)
        .bind(code_hash)
        .execute(&mut **tx)
        .await?;
        return Ok(RedeemError::BadCode);
    }
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM family_code_attempts")
        .fetch_one(&mut **tx)
        .await?;
    if live >= MAX_CODE_BUDGETS {
        return Ok(RedeemError::Blocked);
    }
    sqlx::query(
        "INSERT INTO family_code_attempts (code_hash, failures, window_started_at, updated_at)
         VALUES (?, 1, ?, ?)",
    )
    .bind(code_hash)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(RedeemError::BadCode)
}

async fn link_info(
    tx: &mut Transaction<'_, Sqlite>,
    link_id: i64,
    user_id: i64,
) -> Result<LinkInfo, sqlx::Error> {
    sqlx::query_as(
        "SELECT l.id,
                CASE WHEN l.parent_id = ? THEN 'parent' ELSE 'child' END AS direction,
                l.status,
                l.default_permission AS permission,
                l.notify_on_complete,
                CASE WHEN l.parent_id = ? THEN c.name ELSE p.name END AS other_name,
                l.created_at, l.accepted_at, l.revoked_at, l.pending_expires_at
         FROM family_links l
         JOIN users p ON p.id = l.parent_id
         JOIN users c ON c.id = l.child_id
         WHERE l.id = ? AND (l.parent_id = ? OR l.child_id = ?)",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(link_id)
    .bind(user_id)
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await
}

/// Every link the caller is on, in both directions, newest first.
pub async fn links_for(db: &Db, user_id: i64) -> Result<Vec<LinkInfo>, sqlx::Error> {
    sqlx::query_as(
        "SELECT l.id,
                CASE WHEN l.parent_id = ? THEN 'parent' ELSE 'child' END AS direction,
                l.status,
                l.default_permission AS permission,
                l.notify_on_complete,
                CASE WHEN l.parent_id = ? THEN c.name ELSE p.name END AS other_name,
                l.created_at, l.accepted_at, l.revoked_at, l.pending_expires_at
         FROM family_links l
         JOIN users p ON p.id = l.parent_id
         JOIN users c ON c.id = l.child_id
         WHERE l.parent_id = ? OR l.child_id = ?
         ORDER BY l.created_at DESC, l.id DESC",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(user_id)
    .bind(user_id)
    .fetch_all(db)
    .await
}

/// The link states that grant access. A pending or revoked row never appears here, so item
/// routes built on this seam cannot leak anything before the child accepts or after a revoke.
pub async fn permission_for(
    db: &Db,
    parent_id: i64,
    child_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT default_permission FROM family_links
         WHERE parent_id = ? AND child_id = ? AND status = 'active'",
    )
    .bind(parent_id)
    .bind(child_id)
    .fetch_optional(db)
    .await
}

/// Active children a parent may assign to. Nothing is returned for pending or revoked links.
pub async fn active_children(db: &Db, parent_id: i64) -> Result<Vec<ActiveParty>, sqlx::Error> {
    sqlx::query_as(
        "SELECT l.child_id AS user_id, c.name, l.default_permission AS permission
         FROM family_links l JOIN users c ON c.id = l.child_id
         WHERE l.parent_id = ? AND l.status = 'active'
         ORDER BY c.name, l.child_id",
    )
    .bind(parent_id)
    .fetch_all(db)
    .await
}

/// Active parents a child has accepted. Nothing is returned for pending or revoked links.
pub async fn active_parents(db: &Db, child_id: i64) -> Result<Vec<ActiveParty>, sqlx::Error> {
    sqlx::query_as(
        "SELECT l.parent_id AS user_id, p.name, l.default_permission AS permission
         FROM family_links l JOIN users p ON p.id = l.parent_id
         WHERE l.child_id = ? AND l.status = 'active'
         ORDER BY p.name, l.parent_id",
    )
    .bind(child_id)
    .fetch_all(db)
    .await
}

/// The permission an item actually grants: its own override when set, otherwise the active
/// link's default. `None` when the item has no active parent link, which is how the user's own
/// items (and anything left after a revoke) stay fully editable.
pub fn effective_permission(
    override_level: Option<&str>,
    default_permission: Option<&str>,
) -> Option<String> {
    default_permission.map(|default| override_level.unwrap_or(default).to_string())
}

/// Fill [`Item::effective_permission`] for the child's pull.
///
/// This deliberately reads the active links separately and post-processes the rows fetched by
/// the indexed `(user_id, seq)` query. Joining `family_links` into `pull_sql` would change that
/// plan; a parent's default is a small, per-child lookup either way.
pub async fn apply_effective_permissions(
    conn: &mut SqliteConnection,
    child_id: i64,
    items: &mut [Item],
) -> Result<(), sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT parent_id, default_permission FROM family_links
         WHERE child_id = ? AND status = 'active'",
    )
    .bind(child_id)
    .fetch_all(&mut *conn)
    .await?;
    let defaults: HashMap<i64, String> = rows.into_iter().collect();
    for item in items {
        item.effective_permission = match item.created_by {
            Some(creator) if creator != child_id => effective_permission(
                item.permission_override.as_deref(),
                defaults.get(&creator).map(String::as_str),
            ),
            _ => None,
        };
    }
    Ok(())
}

/// A parent's listing of the items it assigned to one child, with the effective permission
/// resolved exactly as the child sees it. `None` when the link is not active, so a route can
/// answer 404 instead of leaking items after a revoke.
pub async fn assigned_items(
    db: &Db,
    parent_id: i64,
    child_id: i64,
) -> Result<Option<Vec<Item>>, sqlx::Error> {
    let Some(default_permission) = permission_for(db, parent_id, child_id).await? else {
        return Ok(None);
    };
    let mut conn = db.acquire().await?;
    // `items_user_created_by_seq` serves both the filter and the order.
    let mut items: Vec<Item> = sqlx::query_as(&format!(
        "SELECT {ITEM_COLS} FROM items
         WHERE user_id = ? AND created_by = ?
         ORDER BY seq"
    ))
    .bind(child_id)
    .bind(parent_id)
    .fetch_all(&mut *conn)
    .await?;
    for item in &mut items {
        item.effective_permission = effective_permission(
            item.permission_override.as_deref(),
            Some(&default_permission),
        );
    }
    Ok(Some(items))
}

/// Advance the sequence of every item one parent created for one child, without touching
/// `updated_at`.
///
/// The child's pull is cursor-based, so a changed link default only reaches the phone if the
/// affected rows move past its cursor. Bumping `updated_at` instead would make the permission
/// change look like a real edit and could win last-write-wins against a genuine child edit.
/// Fresh sequence numbers are allocated in one block and assigned in stored sequence order,
/// so the bumped rows stay ordered and every one is strictly newer than the caller's cursor.
async fn bump_creator_items_seq(
    tx: &mut Transaction<'_, Sqlite>,
    child_id: i64,
    parent_id: i64,
) -> Result<u64, sqlx::Error> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE user_id = ? AND created_by = ?")
            .bind(child_id)
            .bind(parent_id)
            .fetch_one(&mut **tx)
            .await?;
    if count == 0 {
        return Ok(0);
    }
    let last: i64 =
        sqlx::query_scalar("UPDATE meta SET val = val + ? WHERE key = 'seq' RETURNING val")
            .bind(count)
            .fetch_one(&mut **tx)
            .await?;
    let base = last - count;
    let updated = sqlx::query(
        "WITH ranked AS (
             SELECT id, ROW_NUMBER() OVER (ORDER BY seq, id) AS rn
             FROM items WHERE user_id = ? AND created_by = ?
         )
         UPDATE items
         SET seq = ? + (SELECT rn FROM ranked WHERE ranked.id = items.id)
         WHERE user_id = ? AND created_by = ?",
    )
    .bind(child_id)
    .bind(parent_id)
    .bind(base)
    .bind(child_id)
    .bind(parent_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    debug_assert_eq!(updated as i64, count);
    Ok(updated)
}

/// Why a parent could not change its link's configuration.
#[derive(Debug, PartialEq, Eq)]
pub enum LinkUpdateOutcome {
    Updated(LinkInfo),
    /// No such link for this caller. A child, or an unowned id, sees this, so ids are not
    /// confirmed.
    NotFound,
    /// Pending or revoked links grant nothing and cannot be configured.
    NotActive,
    InvalidPermission,
}

/// Change the link's default permission and/or completion alerts. Only the parent may do it,
/// and only while the link is active. Changing the default bumps the sequence of the parent's
/// items under the child (see [`bump_creator_items_seq`]); changing only the alert toggle does
/// not, because it does not change any item's effective permission.
pub async fn update_link(
    db: &Db,
    parent_id: i64,
    link_id: i64,
    default_permission: Option<&str>,
    notify_on_complete: Option<bool>,
) -> Result<LinkUpdateOutcome, sqlx::Error> {
    if let Some(level) = default_permission {
        if !valid_permission(level) {
            return Ok(LinkUpdateOutcome::InvalidPermission);
        }
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, i64, String, bool)> = sqlx::query_as(
        "SELECT status, child_id, default_permission, notify_on_complete
         FROM family_links WHERE id = ? AND parent_id = ?",
    )
    .bind(link_id)
    .bind(parent_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, child_id, current_permission, current_notify)) = row else {
        return Ok(LinkUpdateOutcome::NotFound);
    };
    if status != "active" {
        return Ok(LinkUpdateOutcome::NotActive);
    }
    let permission = default_permission.unwrap_or(&current_permission);
    let permission_changed = permission != current_permission;
    let notify = notify_on_complete.unwrap_or(current_notify);
    sqlx::query(
        "UPDATE family_links SET default_permission = ?, notify_on_complete = ?
         WHERE id = ? AND parent_id = ? AND status = 'active'",
    )
    .bind(permission)
    .bind(notify)
    .bind(link_id)
    .bind(parent_id)
    .execute(&mut *tx)
    .await?;
    if permission_changed {
        bump_creator_items_seq(&mut tx, child_id, parent_id).await?;
    }
    let link = link_info(&mut tx, link_id, parent_id).await?;
    tx.commit().await?;
    Ok(LinkUpdateOutcome::Updated(link))
}

/// Why a parent could not set an item's override.
#[derive(Debug, PartialEq, Eq)]
pub enum OverrideOutcome {
    Updated {
        effective_permission: String,
    },
    /// No active link with this child, or no such item on it.
    NotFound,
    NotActive,
    InvalidPermission,
    /// The item belongs to the child, not to this parent.
    NotCreator,
}

/// Set or clear one item's permission override.
///
/// This is the write helper the TRACK-57 parent item routes call. It enforces the whole rule
/// in one transaction: the caller must have an active link to the child, the enum must be
/// valid, and only the parent that created the item may change it. Like a link default change,
/// a new override bumps the item's sequence without moving `updated_at`, so the child's normal
/// pull delivers the new effective permission and a permission change never competes with a
/// real edit in last-write-wins.
pub async fn set_item_override(
    db: &Db,
    parent_id: i64,
    child_id: i64,
    item_id: &str,
    permission: Option<&str>,
) -> Result<OverrideOutcome, sqlx::Error> {
    if let Some(level) = permission {
        if !valid_permission(level) {
            return Ok(OverrideOutcome::InvalidPermission);
        }
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT status, default_permission FROM family_links
         WHERE parent_id = ? AND child_id = ?",
    )
    .bind(parent_id)
    .bind(child_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, default_permission)) = row else {
        return Ok(OverrideOutcome::NotFound);
    };
    if status != "active" {
        return Ok(OverrideOutcome::NotActive);
    }
    let creator: Option<Option<i64>> =
        sqlx::query_scalar("SELECT created_by FROM items WHERE user_id = ? AND id = ?")
            .bind(child_id)
            .bind(item_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(creator) = creator else {
        return Ok(OverrideOutcome::NotFound);
    };
    if creator != Some(parent_id) {
        return Ok(OverrideOutcome::NotCreator);
    }
    let seq = crate::store::next_seq(&mut tx).await?;
    sqlx::query(
        "UPDATE items SET permission_override = ?, seq = ?
         WHERE user_id = ? AND id = ? AND created_by = ?",
    )
    .bind(permission)
    .bind(seq)
    .bind(child_id)
    .bind(item_id)
    .bind(parent_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(OverrideOutcome::Updated {
        effective_permission: permission.unwrap_or(&default_permission).to_string(),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum AcceptOutcome {
    Accepted,
    /// No such link for this caller. The route answers 404 so an unowned id is not confirmed.
    NotFound,
    /// Already accepted or already revoked.
    NotPending,
    /// Pending, but its acceptance window has passed.
    Expired,
}

/// Accept a pending, unexpired link. Only the child may do this.
pub async fn accept(db: &Db, child_id: i64, link_id: i64) -> Result<AcceptOutcome, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT status, pending_expires_at FROM family_links WHERE id = ? AND child_id = ?",
    )
    .bind(link_id)
    .bind(child_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, pending_expires_at)) = row else {
        return Ok(AcceptOutcome::NotFound);
    };
    if status != "pending" {
        return Ok(AcceptOutcome::NotPending);
    }
    let now = now_ms();
    if pending_expires_at.is_none_or(|expires_at| expires_at <= now) {
        return Ok(AcceptOutcome::Expired);
    }
    let accepted = sqlx::query(
        "UPDATE family_links SET status = 'active', accepted_at = ?, pending_expires_at = NULL
         WHERE id = ? AND child_id = ? AND status = 'pending' AND pending_expires_at > ?",
    )
    .bind(now)
    .bind(link_id)
    .bind(child_id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(if accepted == 1 {
        AcceptOutcome::Accepted
    } else {
        AcceptOutcome::NotPending
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Revoked {
    Revoked,
    AlreadyRevoked,
    /// No such link for this caller (or no such id at all).
    NotFound,
}

async fn revoke_where(db: &Db, link_id: i64, caller: Option<i64>) -> Result<Revoked, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, i64, i64)> = match caller {
        Some(user_id) => {
            sqlx::query_as(
                "SELECT status, parent_id, child_id FROM family_links
             WHERE id = ? AND (parent_id = ? OR child_id = ?)",
            )
            .bind(link_id)
            .bind(user_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?
        }
        None => {
            sqlx::query_as("SELECT status, parent_id, child_id FROM family_links WHERE id = ?")
                .bind(link_id)
                .fetch_optional(&mut *tx)
                .await?
        }
    };
    let Some((status, parent_id, child_id)) = row else {
        return Ok(Revoked::NotFound);
    };
    if status == "revoked" {
        tx.commit().await?;
        return Ok(Revoked::AlreadyRevoked);
    }
    let updated = match caller {
        Some(user_id) => {
            sqlx::query(
                "UPDATE family_links SET status = 'revoked', revoked_at = ?
             WHERE id = ? AND (parent_id = ? OR child_id = ?) AND status != 'revoked'",
            )
            .bind(now_ms())
            .bind(link_id)
            .bind(user_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?
        }
        None => {
            sqlx::query(
                "UPDATE family_links SET status = 'revoked', revoked_at = ?
             WHERE id = ? AND status != 'revoked'",
            )
            .bind(now_ms())
            .bind(link_id)
            .execute(&mut *tx)
            .await?
        }
    };
    let revoked = updated.rows_affected() == 1;
    if revoked {
        // DESIGN.md Revocation: the items the parent added stay on the child's list as the
        // child's own. Clearing provenance here means no later query can mistake them for
        // parent data, even before it rechecks the link status.
        clear_provenance(&mut tx, child_id, parent_id).await?;
    }
    tx.commit().await?;
    Ok(if revoked {
        Revoked::Revoked
    } else {
        Revoked::AlreadyRevoked
    })
}

/// Hand one parent's assigned items to the child permanently.
async fn clear_provenance(
    tx: &mut Transaction<'_, Sqlite>,
    child_id: i64,
    parent_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE items SET created_by = ?, permission_override = NULL
         WHERE user_id = ? AND created_by = ?",
    )
    .bind(child_id)
    .bind(child_id)
    .bind(parent_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Revoke a link as either party. A link the caller is not on answers `NotFound`.
pub async fn revoke(db: &Db, user_id: i64, link_id: i64) -> Result<Revoked, sqlx::Error> {
    revoke_where(db, link_id, Some(user_id)).await
}

/// Revoke any link by id, for the server owner's CLI.
pub async fn revoke_by_id(db: &Db, link_id: i64) -> Result<AdminRevoked, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, i64, i64, String, String)> = sqlx::query_as(
        "SELECT l.status, l.parent_id, l.child_id, p.name, c.name FROM family_links l
         JOIN users p ON p.id = l.parent_id JOIN users c ON c.id = l.child_id
         WHERE l.id = ?",
    )
    .bind(link_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, parent_id, child_id, parent, child)) = row else {
        return Ok(AdminRevoked::NotFound);
    };
    if status == "revoked" {
        tx.commit().await?;
        return Ok(AdminRevoked::AlreadyRevoked);
    }
    sqlx::query("UPDATE family_links SET status = 'revoked', revoked_at = ? WHERE id = ? AND status != 'revoked'")
        .bind(now_ms())
        .bind(link_id)
        .execute(&mut *tx)
        .await?;
    clear_provenance(&mut tx, child_id, parent_id).await?;
    tx.commit().await?;
    Ok(AdminRevoked::Revoked { parent, child })
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdminRevoked {
    Revoked { parent: String, child: String },
    AlreadyRevoked,
    NotFound,
}

/// Every link, with both display names, for `family list`.
pub async fn list_all(db: &Db) -> Result<Vec<AdminLinkInfo>, sqlx::Error> {
    sqlx::query_as(
        "SELECT l.id, p.name AS parent_name, c.name AS child_name, l.status,
                l.default_permission AS permission, l.created_at, l.pending_expires_at
         FROM family_links l
         JOIN users p ON p.id = l.parent_id
         JOIN users c ON c.id = l.child_id
         ORDER BY l.id",
    )
    .fetch_all(db)
    .await
}

/// The child's local date. Parent writes are validated against it and assigned-item stats are
/// anchored to it, so a parent in another timezone sees the child's day, not their own.
async fn local_today(conn: &mut SqliteConnection, user_id: i64) -> Result<NaiveDate, sqlx::Error> {
    let timezone: Option<String> =
        sqlx::query_scalar("SELECT timezone FROM user_settings WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&mut *conn)
            .await?;
    let tz: Tz = timezone
        .as_deref()
        .and_then(|value| value.parse().ok())
        .unwrap_or(chrono_tz::UTC);
    Ok(Utc::now().with_timezone(&tz).date_naive())
}

/// The editable fields of a parent item write. `None` means leave the stored value alone;
/// the nested `Option` on a nullable field distinguishes "absent" from an explicit null.
#[derive(Debug, Clone, Default)]
pub struct ItemPatch {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub weekdays: Option<i64>,
    pub remind_time: Option<Option<String>>,
    pub points: Option<i64>,
    pub due_date: Option<Option<String>>,
    pub sort: Option<i64>,
    pub active: Option<bool>,
}

impl ItemPatch {
    pub fn is_empty(&self) -> bool {
        self.kind.is_none()
            && self.name.is_none()
            && self.weekdays.is_none()
            && self.remind_time.is_none()
            && self.points.is_none()
            && self.due_date.is_none()
            && self.sort.is_none()
            && self.active.is_none()
    }

    fn apply(&self, item: &mut Item) {
        if let Some(kind) = &self.kind {
            item.kind = kind.clone();
        }
        if let Some(name) = &self.name {
            item.name = name.clone();
        }
        if let Some(weekdays) = self.weekdays {
            item.weekdays = weekdays;
        }
        if let Some(remind_time) = &self.remind_time {
            item.remind_time = remind_time.clone();
        }
        if let Some(points) = self.points {
            item.points = points;
        }
        if let Some(due_date) = &self.due_date {
            item.due_date = due_date.clone();
        }
        if let Some(sort) = self.sort {
            item.sort = sort;
        }
        if let Some(active) = self.active {
            item.active = active;
        }
    }
}

/// Outcome of a parent item write. Every `NotFound` is deliberately indistinguishable from a
/// child id the caller is not linked to, an item the child owns, and an item that does not
/// exist, so an id is never confirmed.
#[derive(Debug)]
pub enum ItemWriteOutcome {
    Created(Item),
    Updated(Item),
    Deleted,
    NotFound,
    /// The child (or another parent) already holds this id.
    Collision,
    Invalid(String),
    LimitReached(String),
}

/// Create an item on an active child's list. `created_by` is the authenticated parent, the
/// timestamp is the server's, and the item counts against the child's item quota. The id is
/// refused if the child (or another parent) already uses it: a parent write may never overwrite
/// an existing row.
pub async fn create_item(
    db: &Db,
    parent_id: i64,
    child_id: i64,
    item: &Item,
    limits: &Limits,
) -> Result<ItemWriteOutcome, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let permission: Option<String> = sqlx::query_scalar(
        "SELECT default_permission FROM family_links
         WHERE parent_id = ? AND child_id = ? AND status = 'active'",
    )
    .bind(parent_id)
    .bind(child_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(permission) = permission else {
        return Ok(ItemWriteOutcome::NotFound);
    };
    let today = local_today(&mut tx, child_id).await?;
    if let Err(message) = item.validate(today) {
        return Ok(ItemWriteOutcome::Invalid(message));
    }
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE user_id = ? AND id = ?)")
            .bind(child_id)
            .bind(&item.id)
            .fetch_one(&mut *tx)
            .await?;
    if exists {
        return Ok(ItemWriteOutcome::Collision);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE user_id = ?")
        .bind(child_id)
        .fetch_one(&mut *tx)
        .await?;
    if count >= limits.max_items {
        return Ok(ItemWriteOutcome::LimitReached(format!(
            "item limit reached ({} per account)",
            limits.max_items
        )));
    }
    let now = now_ms();
    let created_on = first_seen_date(&mut tx, child_id).await?;
    let seq = next_seq(&mut tx).await?;
    let name = item.name.trim();
    sqlx::query(
        "INSERT INTO items (user_id, id, kind, name, weekdays, remind_time, points, due_date,
                            sort, active, updated_at, deleted_at, created_on, created_by,
                            permission_override, seq)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, NULL, ?)",
    )
    .bind(child_id)
    .bind(&item.id)
    .bind(&item.kind)
    .bind(name)
    .bind(item.weekdays)
    .bind(&item.remind_time)
    .bind(item.points)
    .bind(&item.due_date)
    .bind(item.sort)
    .bind(item.active)
    .bind(now)
    .bind(&created_on)
    .bind(parent_id)
    .bind(seq)
    .execute(&mut *tx)
    .await?;
    let current = LedgerItem::from_write(item, Some(created_on.clone()));
    apply_item_ledger(&mut tx, child_id, &item.id, None, &current).await?;
    let stored = Item {
        id: item.id.clone(),
        kind: item.kind.clone(),
        name: name.to_string(),
        weekdays: item.weekdays,
        remind_time: item.remind_time.clone(),
        points: item.points,
        due_date: item.due_date.clone(),
        sort: item.sort,
        active: item.active,
        updated_at: now,
        deleted_at: None,
        created_on: Some(created_on),
        created_by: Some(parent_id),
        permission_override: None,
        effective_permission: Some(permission),
        seq,
    };
    tx.commit().await?;
    Ok(ItemWriteOutcome::Created(stored))
}

/// Edit one item the caller created for an active child. The stored `created_by` is the only
/// authority; an item the child owns is a 404, and a deleted item is not editable again. The
/// server timestamp replaces whatever the parent's clock might claim.
pub async fn update_item(
    db: &Db,
    parent_id: i64,
    child_id: i64,
    item_id: &str,
    patch: &ItemPatch,
) -> Result<ItemWriteOutcome, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let permission: Option<String> = sqlx::query_scalar(
        "SELECT default_permission FROM family_links
         WHERE parent_id = ? AND child_id = ? AND status = 'active'",
    )
    .bind(parent_id)
    .bind(child_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(permission) = permission else {
        return Ok(ItemWriteOutcome::NotFound);
    };
    let existing: Option<Item> = sqlx::query_as(&format!(
        "SELECT {ITEM_COLS} FROM items WHERE user_id = ? AND id = ?"
    ))
    .bind(child_id)
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(mut stored) = existing else {
        return Ok(ItemWriteOutcome::NotFound);
    };
    if stored.created_by != Some(parent_id) || stored.deleted_at.is_some() {
        return Ok(ItemWriteOutcome::NotFound);
    }
    let previous_item = stored.clone();
    patch.apply(&mut stored);
    stored.name = stored.name.trim().to_string();
    let today = local_today(&mut tx, child_id).await?;
    if let Err(message) = stored.validate(today) {
        return Ok(ItemWriteOutcome::Invalid(message));
    }
    let now = now_ms();
    stored.updated_at = now;
    stored.deleted_at = None;
    let seq = next_seq(&mut tx).await?;
    sqlx::query(
        "UPDATE items SET kind = ?, name = ?, weekdays = ?, remind_time = ?, points = ?,
                          due_date = ?, sort = ?, active = ?, updated_at = ?, seq = ?
         WHERE user_id = ? AND id = ? AND created_by = ?",
    )
    .bind(&stored.kind)
    .bind(&stored.name)
    .bind(stored.weekdays)
    .bind(&stored.remind_time)
    .bind(stored.points)
    .bind(&stored.due_date)
    .bind(stored.sort)
    .bind(stored.active)
    .bind(now)
    .bind(seq)
    .bind(child_id)
    .bind(item_id)
    .bind(parent_id)
    .execute(&mut *tx)
    .await?;
    let previous = LedgerItem::from_item(&previous_item);
    let current = LedgerItem::from_item(&stored);
    apply_item_ledger(&mut tx, child_id, item_id, Some(&previous), &current).await?;
    stored.seq = seq;
    stored.effective_permission = Some(
        effective_permission(stored.permission_override.as_deref(), Some(&permission))
            .expect("an active link always yields a level"),
    );
    tx.commit().await?;
    Ok(ItemWriteOutcome::Updated(stored))
}

/// Delete (tombstone) one item the caller created for an active child. The ledger drops the
/// XP its completions earned, exactly as the child deleting their own item would.
pub async fn delete_item(
    db: &Db,
    parent_id: i64,
    child_id: i64,
    item_id: &str,
) -> Result<ItemWriteOutcome, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let active: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM family_links
         WHERE parent_id = ? AND child_id = ? AND status = 'active'",
    )
    .bind(parent_id)
    .bind(child_id)
    .fetch_optional(&mut *tx)
    .await?;
    if active.is_none() {
        return Ok(ItemWriteOutcome::NotFound);
    }
    let existing: Option<Item> = sqlx::query_as(&format!(
        "SELECT {ITEM_COLS} FROM items WHERE user_id = ? AND id = ?"
    ))
    .bind(child_id)
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(stored) = existing else {
        return Ok(ItemWriteOutcome::NotFound);
    };
    if stored.created_by != Some(parent_id) || stored.deleted_at.is_some() {
        return Ok(ItemWriteOutcome::NotFound);
    }
    let now = now_ms();
    let seq = next_seq(&mut tx).await?;
    sqlx::query(
        "UPDATE items SET deleted_at = ?, updated_at = ?, seq = ?
         WHERE user_id = ? AND id = ? AND created_by = ?",
    )
    .bind(now)
    .bind(now)
    .bind(seq)
    .bind(child_id)
    .bind(item_id)
    .bind(parent_id)
    .execute(&mut *tx)
    .await?;
    let previous = LedgerItem::from_item(&stored);
    let current = LedgerItem {
        deleted_at: Some(now),
        ..previous.clone()
    };
    apply_item_ledger(&mut tx, child_id, item_id, Some(&previous), &current).await?;
    tx.commit().await?;
    Ok(ItemWriteOutcome::Deleted)
}

/// Queue one completion alert for a parent, inside the child's sync transaction.
///
/// The insert is gated on an active link with `notify_on_complete` on, so a toggle that is off
/// (or a link that was revoked) enqueues nothing. `INSERT OR IGNORE` against the unique
/// (parent, child, item, date) key makes a retried sync idempotent, and doing it in the same
/// transaction as the accepted completion means there is no window where the completion is
/// stored but its alert is lost. The date is the canonical `YYYY-MM-DD` the statistics ledger
/// uses, so two spellings of one completion cannot create two alerts.
pub async fn enqueue_completion_alert(
    tx: &mut Transaction<'_, Sqlite>,
    parent_id: i64,
    child_id: i64,
    item_id: &str,
    date: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO family_alerts (parent_id, child_id, item_id, date, created_at)
         SELECT ?, ?, ?, ?, ?
         WHERE EXISTS (
             SELECT 1 FROM family_links
             WHERE parent_id = ? AND child_id = ?
               AND status = 'active' AND notify_on_complete = 1
         )",
    )
    .bind(parent_id)
    .bind(child_id)
    .bind(item_id)
    .bind(date)
    .bind(now)
    .bind(parent_id)
    .bind(child_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Everything a parent may read about one active child: only the items that parent created,
/// only their completions, and assigned-item XP. Filtering happens in SQL before the shared
/// rules run, so the child's own items, settings, devices, tokens, completions and full stats
/// never enter the result.
#[derive(Debug)]
pub struct AssignedProgress {
    pub items: Vec<Item>,
    pub completions: Vec<Completion>,
    pub stats: xp::Stats,
}

/// Assigned item XP, computed with the shared rules over the filtered set only.
///
/// Perfect-day and streak values are deliberately absent: they depend on the child's whole
/// schedule, so returning them (even filtered) would reveal whether private items were done.
/// The item XP component is the same arithmetic `xp::compute` uses.
fn assigned_stats(items: &[Item], completions: &[Completion], today: NaiveDate) -> xp::Stats {
    let filtered = xp::compute(items, completions, today);
    let item_xp = filtered.xp - xp::PERFECT_DAY_BONUS * filtered.perfect_days.len() as i64;
    xp::Stats::from_parts(item_xp, Vec::new(), BTreeMap::new())
}

/// Read one active child's assigned items, their completions and their assigned XP in one
/// deferred snapshot. `None` when the link is not active, so the route answers 404.
pub async fn assigned_progress(
    db: &Db,
    parent_id: i64,
    child_id: i64,
) -> Result<Option<AssignedProgress>, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN DEFERRED").await?;
    let permission: Option<String> = sqlx::query_scalar(
        "SELECT default_permission FROM family_links
         WHERE parent_id = ? AND child_id = ? AND status = 'active'",
    )
    .bind(parent_id)
    .bind(child_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(permission) = permission else {
        return Ok(None);
    };
    let today = local_today(&mut tx, child_id).await?;
    let mut items: Vec<Item> = sqlx::query_as(&format!(
        "SELECT {ITEM_COLS} FROM items
         WHERE user_id = ? AND created_by = ?
         ORDER BY seq"
    ))
    .bind(child_id)
    .bind(parent_id)
    .fetch_all(&mut *tx)
    .await?;
    for item in &mut items {
        item.effective_permission =
            effective_permission(item.permission_override.as_deref(), Some(&permission));
    }
    let completions: Vec<Completion> = sqlx::query_as(&format!(
        "SELECT {COMPLETION_COLS} FROM completions
         WHERE user_id = ? AND item_id IN (
             SELECT id FROM items WHERE user_id = ? AND created_by = ?
         )
         ORDER BY item_id, date"
    ))
    .bind(child_id)
    .bind(child_id)
    .bind(parent_id)
    .fetch_all(&mut *tx)
    .await?;
    let stats = assigned_stats(&items, &completions, today);
    tx.commit().await?;
    Ok(Some(AssignedProgress {
        items,
        completions,
        stats,
    }))
}

#[cfg(test)]
mod tests {
    use super::{new_code, CODE_TTL_MS};

    #[test]
    fn codes_have_twelve_symbols_and_a_fifteen_minute_lifetime() {
        let code = new_code();
        assert_eq!(code.replace('-', "").len(), 12, "{code}");
        assert_eq!(code.len(), 14, "{code}");
        assert_eq!(CODE_TTL_MS, 900_000);
    }
}
