//! Family links: a child offers a short-lived single-use code, a parent redeems it into a
//! pending link, and only the child can accept it. Either side can revoke a link at any time.
//!
//! The child is never linked unilaterally: redemption only creates a `pending` row, and the
//! accept route is the only path to `active`. Nothing reads a link as permission unless its
//! status is `active`, which is the seam later item routes use; a pending or revoked link
//! therefore grants no items and no parent reads.

use rand::Rng;
use sqlx::{Sqlite, Transaction};

use crate::auth::{hash, normalize_code, CallerIdentity, CODE_ALPHABET};
use crate::db::Db;
use crate::store::now_ms;

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
    let Some((status, _parent, _child)) = row else {
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
    tx.commit().await?;
    Ok(if updated.rows_affected() == 1 {
        Revoked::Revoked
    } else {
        Revoked::AlreadyRevoked
    })
}

/// Revoke a link as either party. A link the caller is not on answers `NotFound`.
pub async fn revoke(db: &Db, user_id: i64, link_id: i64) -> Result<Revoked, sqlx::Error> {
    revoke_where(db, link_id, Some(user_id)).await
}

/// Revoke any link by id, for the server owner's CLI.
pub async fn revoke_by_id(db: &Db, link_id: i64) -> Result<AdminRevoked, sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT l.status, p.name, c.name FROM family_links l
         JOIN users p ON p.id = l.parent_id JOIN users c ON c.id = l.child_id
         WHERE l.id = ?",
    )
    .bind(link_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, parent, child)) = row else {
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
