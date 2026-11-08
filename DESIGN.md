# Bread Tracker design

Separate from the Nextcloud Chores calendar and the Gotify cron reminders. Server and
database live on a server you host; the Android app (Flutter) is offline-first and syncs to it.
The server is shared: each person has their own account, and isolation is per user (see Users
below). The one exception is an explicit family link that the child accepts (see Family): a
linked parent can assign items to the child's list and see only those items and their progress.
Without a link there is no sharing, and a child is fully usable with no parent.

## Rules (shared by server `server/src/xp.rs` and the app; vectors in `spec/xp_vectors.json`)

- XP per completion is the item's `points` (defaults: chore 10, habit 15, to-do 20). A date counts once per item.
- A **perfect day** is a date with at least one scheduled chore or habit where every scheduled chore and habit is done. It adds 50 XP. To-dos are never scheduled, so they never make or break a perfect day. Scheduled means active and the date's weekday bit is set (`weekdays` bit0 = Monday .. bit6 = Sunday).
- 120 XP per level; level = xp / 120 + 1. Ranks, one per level and clamped at the last: Flour, Dough, Proofing, Rising, Baked, Golden Crust, Sourdough Master.
- A habit **streak** is the run of consecutive completed days ending today, or ending yesterday when today is not done yet.
- Un-ticked completions and completions of deleted items earn nothing.
- Perfect days are evaluated against the current item definitions, not the schedule as it was in the past.

## Sync

- Rows: `items` (key `user_id` + `id`) and `completions` (key `user_id` + `item_id` + `date`); every sync request only sees the caller's own rows (the family routes in Family are separate). Every row has `updated_at` (client unix ms) and `deleted_at` (tombstone, so an un-tick or delete syncs).
- Server assigns a monotonic `seq` to every accepted write. The cursor is a `seq`, so clock skew cannot lose rows.
- `POST /sync {since, items, completions}` pushes local changes then returns every row with `seq > since` plus the new `cursor`, in one round trip. `GET /sync?since=` is pull only.
- Conflicts: last write wins by `updated_at`; ties keep the server row. Timestamps more than a day in the future are clamped.
- Batches are limited to 5000 rows and 2 MiB. A completion dated more than 7 days ahead of the server's UTC date, or before 2020, is rejected; `completed_at` is clamped like `updated_at`.
- Housekeeping runs hourly in the scheduler: `reminders_sent` older than two days and expired pairing codes are deleted. Tombstoned items and completions are retained because the current cursor protocol has no per-device acknowledgement or forced-full-resync contract; pruning them could strand an offline client. XP and streak statistics use all retained history. A bounded-history or retention policy requires an explicit backward-compatible protocol/product decision before implementation.

## Auth

Every account has one or more devices. A device holds a bearer token; only its SHA-256 is stored, and the token identifies exactly one device and so exactly one user. There are two ways to get a device:

- **Join with an invite** (a new account): `POST /signup {invite, name, device_name?, timezone?}`, public, see "Invites and signup" below.
- **Pair with a code** (another device for an existing account): `bread-tracker-server pair [--user <name>]` prints a one-time code (valid 15 minutes, stored hashed, bound to that user; the admin's when no user is given), or a signed-in device asks `POST /pair-code` for one for its own account. `POST /pair {code, device_name}` returns a bearer token once, plus the account's `user_id` and `name`.

Public authentication is limited per client address: five failed pairing or signup attempts in ten minutes, held in SQLite so it survives a restart. Successful attempts are not counted, and one blocked address does not affect another. The client address is the TCP peer, or the `X-Forwarded-For` client when the peer is listed in `BREADTRACKER_TRUSTED_PROXIES`. All routes except `/healthz`, `/pair` and `/signup` need `Authorization: Bearer <token>`; every authenticated request is also limited per device token (see Limits).

Devices can be listed and revoked: `bread-tracker-server devices [--user <name>]` prints id, owner, name, pairing and last-seen time and whether push is registered; `bread-tracker-server revoke <id>` deletes a device by id or unique prefix (at least 4 characters). A revoked token gets 401 on its next request, as does the token of a removed user; a disabled user's tokens get 403. The app has a Sign out action in Settings that calls `DELETE /device` (revokes only the calling device), forgets the token and cursor, and optionally deletes local data. When any request gets 401 the app forgets the dead token and shows the pairing screen again, keeping local data. Tokens still do not expire or rotate.

The first user, the admin, is the original single owner: migration 0002 gave them all pre-existing data. There is no HTTP route that grants or uses admin powers; administration is the command line on the server.

## Reminders

A scheduler ticks every 30 s and evaluates each user in that user's own timezone (UTC for new users; an existing household keeps its Australia/Perth setting). Each user's "today", reminder times and quiet hours use their zone; the app stamps completions with the phone's own calendar date, so the two only agree when the phone is in the user's configured zone. The app's Settings has a Reminders section that reads and writes `GET/PUT /settings` (reminders on/off, quiet hours, timezone) and shows a warning when the server's current UTC offset (`utc_offset_minutes` in the settings response) differs from the phone's. It does not change the server zone on its own. An item is due once its local `remind_time` has passed (chores and habits on their weekdays, to-dos on their due date) and stays eligible for 6 hours (`BREADTRACKER_REMINDER_GRACE_MINS`) so a restart does not blast old reminders. It is skipped when already completed today (a to-do counts as done if it was completed on any day), already sent (`reminders_sent`), or inside quiet hours. A reminder whose time falls inside the quiet window is held and released when the window ends, and then has its full grace window from that moment (including the next morning for a window that wraps midnight). Quiet hours need both a start and an end: `PUT /settings` rejects setting only one. Nothing is marked sent when no device has an FCM token, so a reminder can still go out after a device registers. Dead tokens reported by FCM (UNREGISTERED) are cleared.

Messages are FCM HTTP v1 with notification + data (`item_id`, `date`, `kind`), Android channel `reminders`, high priority. Android shows the notification when the app is closed; the app shows the same notification itself when it is in the foreground. Tapping opens the app; there is no notification action that writes a completion.

## Users and migrations

Every owned row belongs to a user: `items`, `completions`, `devices`, `reminders_sent` and `pairing_codes` carry a `user_id` referencing `users(id)` with `ON DELETE CASCADE`, so removing a user removes only their data. Keys are per user: items are `(user_id, id)`, completions `(user_id, item_id, date)`, reminders `(user_id, item_id, date)`. A client pushing an item id that another user also holds therefore creates or updates its own row and cannot touch the other one. Device token hashes stay globally unique. `meta.seq` remains one global counter; each user's own `seq` values have gaps, which is harmless because a cursor only means "everything larger". Sync reads search the `(user_id, seq)` indexes (`UserDb::pull_query_plans` and a test prove it).

**Routes are scoped.** `require_auth` looks the bearer token up in `devices` and puts a `DeviceId` and a `UserId` into the request extensions. The user id always comes from that row, never from the body or query. Every query on `items`, `completions` and `reminders_sent` lives on `store::UserDb`, which holds the id privately and binds it in every statement, so a handler cannot touch those tables without naming whose data it is. `GET/POST /sync`, `/stats`, `/device` and `/push/test` act only on the caller; the sync cursor is the global change counter, so it advances when other users write, and their rows are never returned (the gaps are harmless). The reminder scheduler runs per user (`push::tick` loops over users): a user's reminders come from their own items, completions and sent markers and go only to their own devices, a failure for one user is logged and does not stop the others, and a user without a device does not consume anyone else's reminder.

Per-user settings are stored in `user_settings`: timezone, quiet start/end, and the reminders switch. Migration 0007 copies the existing global preferences to the admin and seeds other/new users with UTC, no quiet hours and reminders enabled; a trigger seeds future users. GET/PUT /settings and /stats use the authenticated user's row. A partial PUT reserves the SQLite writer with BEGIN IMMEDIATE before reading, merges and validates the complete result, then writes and commits that same transaction. Rejected patches change nothing, and disjoint concurrent patches preserve both fields. The scheduler loads those same per-user preferences; invalid stored zones fall back to UTC and a missing settings row fails that user's pass only.

The multi-user stack is integrated on `feature/app` (sync scoping, invites and signup, per-user settings and reminders, the administration CLI, isolation tests, quotas, disabling, deletion and export). That is source acceptance, not a release or deployment approval: the upgrade runbook in `packaging/DEPLOY.md` and a release decision still apply, and a phone can only join a server that already runs this build.

An FCM token identifies one app install, so it belongs to exactly one device across all users. Registering a token another device holds takes it over, and only that token column of the previous holder is cleared, never the device. The caller recheck, the takeover and the assignment run in one `BEGIN IMMEDIATE` transaction: a failure anywhere leaves both devices exactly as they were (tested by failing the caller's assignment after the previous holder was cleared), the device is rechecked inside the transaction so one removed after authentication takes nothing, and concurrent registrations of one token end with one holder. A delayed "unregistered" answer clears only the token FCM rejected, so a token registered meanwhile survives.

Completion references are validated per owner inside the write transaction, before any write: a completion may name an item the caller has (live or tombstoned, so an old offline client whose item was deleted on another device still syncs) or one sent in the same batch. Anything else, including an item only another user holds, rejects the whole batch with nothing written.

User names are 1 to 40 characters: Unicode letters and digits, space, `.`, `_` and `-`, no leading or trailing space. Two names are the same name when their caseless keys are equal (`users::name_key`: canonical decomposition plus Unicode default case folding), so `Älex` and `älex` collide like `Alex` and `alex`. Creation (`users::create`), rename (`users::rename`), lookup (`users::find_by_name`) and the admin naming path all use that key, and a unique index on `users.name_key` enforces it in the database.

SQLite cannot case fold Unicode, so plain SQL has limits. Raw inserts and renames to printable ASCII names keep the key in step (a trigger sets plain lower case). A raw insert or rename to a non-ASCII name is refused unless the key changes with it (migration 0006), because a stale key would hide the user from lookup and let a duplicate in. To rename with SQL while the server is stopped, set `name_key = NULL` together with the new name. At every start, before the listener and the scheduler, `users::reconcile_name_keys` recomputes every user's key from their name inside one `BEGIN IMMEDIATE` transaction: missing and stale keys are repaired, and if two users would share a key nothing is changed and startup fails naming both, never by overwriting a user. SQLite's `NOCASE` is ASCII only and is no longer relied on.

The first user starts as `admin` (migration 0002) and is named once from `BREADTRACKER_ADMIN_NAME`. "Once" is recorded in `users.name_confirmed`, not inferred from the text, so choosing the name `Admin` or `admin` is final too. Restarting with the same or another value changes nothing, and a name that another user already holds stops startup with a clear message instead of renaming anything.

### Per-user settings

Settings are per user, in `user_settings(user_id, timezone, quiet_start, quiet_end, reminders_enabled)`. Migration 0007 moves the old global values to the admin and drops the global `settings` table. A trigger gives every new user a row with the defaults: UTC, no quiet hours, reminders on (the signup flow may set the phone's timezone on that row). `GET/PUT /settings` and `/stats` use the caller's row, so "today" in `/stats` follows the caller's timezone. `PUT /settings` validates the whole result before writing it (timezone known, times `HH:MM` or empty, quiet hours both set or both empty), so a rejected request changes nothing. The scheduler reads each user's own timezone, quiet hours, grace window handling and switch on every tick.

Whether a database is a new installation is decided by recorded provenance, not by the current device count. A household that revokes or loses every device keeps its timezone, quiet hours and reminder history, so zero devices never means "new". Before any migration runs, `db::prepare` records one fact in `install_provenance`: was the database an empty file with no user tables? Migration 0012 writes that marker durably and gives user 1 the neutral UTC default only when the answer is `fresh`; otherwise the values 0007 adopted are left alone. A ledger built without the recording helper (a direct `sqlx::migrate!`, an older binary) has no marker and is classified as `populated`, the conservative choice. The repair runs once, so a later operator choice survives every restart, and a first start that fails inside 0001 keeps its `fresh` marker for the retry.

### Migration ledger and numbering

SQLx identifies a migration by its numeric version and the SHA-384 of its file, records it once, and applies any version it has not recorded in version order. It never reruns a recorded version and refuses to start if a recorded file changed or is missing. Rules for this repository: never edit a migration that has been applied anywhere, versions are unique numbers (a suffix does not make `0003_a` and `0003_b` different), and a database change that a recorded version cannot deliver is a new, later, additive version.

| Version | File | Where | Purpose |
|---|---|---|---|
| 0001 | `0001_init.sql` | default | initial schema |
| 0002 | `0002_users.sql` | default | users, `user_id` and per-user keys; rebuilds five tables |
| 0003 | `0003_hygiene_indexes.sql` | default (TRACK-8) | scheduler and to-do suppression indexes |
| 0004 | `0004_pair_rate_limits.sql` | PR31 (TRACK-5), reserved | durable pairing failure budgets |
| 0005 | `0005_users_forward_repair.sql` | default, published at 6f9986e (treat as applied) | restores 0003's indexes, name key, durable admin naming |
| 0006 | `0006_users_name_key_guard.sql` | default | replaces 0005's rename trigger so a non-ASCII raw rename cannot leave a stale key |
| 0007 | `0007_user_settings.sql` | PR45 | per-user settings and owner/date/partial scheduler indexes |
| 0008 | `0008_invites.sql` | PR46 | single-use invite codes |
| 0009 | `0009_public_auth_limits.sql` | PR46 | shared durable pairing/signup admission |
| 0010 | `0010_user_disabled.sql` | PR51 | non-destructive account disabling |
| 0011 | `0011_authenticated_rate_limits.sql` | PR51 | durable per-device request admission |
| 0012 | `0012_install_provenance.sql` | default (TRACK-82) | records fresh-vs-populated provenance and gives only a genuinely new installation the neutral UTC default |
| 0013 and up | | coordinate downstream before integration | later multi-user and family migrations |

Why 0005 exists. 0003 was applied on deployments before 0002 can run, and 0002 rebuilds `items` and `completions`, which drops every index on them. SQLx will not rerun the recorded 0003, so those indexes would silently disappear and the scheduler would scan the whole history again. 0005 recreates them with `IF NOT EXISTS`, so it is a no-op wherever they survived (a fresh database, where 0002 runs before 0003). It also adds `completions_item_date (item_id, date)`, because the primary key `(user_id, item_id, date)` cannot serve the to-do suppression lookup by item id.

Ledgers this set upgrades, each proven on a populated file database in `server/tests/ledger.rs`: 0001 only; 0001+0003; 0001+0003+0004; 0001+0004; 0001+0002 (the earlier PR43 head); 0001+0002+0003; and a fresh database. Also proven there: published checksums of 0001, 0003 and 0004 are unchanged, a failure inside 0002 rolls back to the untouched legacy database and a retry succeeds, a failure inside 0005 keeps the 0002 result and a retry completes it, an edited recorded migration is refused, and this branch without PR31's 0004 refuses a database that already recorded 0004 rather than guessing.

Failure boundaries. Every migration commits in its own transaction, so a failure rolls back only that migration. If 0005 or 0006 fails, 0002 stays committed (`ledger.rs` proves it): the server refuses to start, the database is a valid intermediate state, and starting again after fixing the cause completes the rest. It is not "the original database": returning to the previous release means restoring a full pre-upgrade copy, taken with all writers stopped. See the runbook in `packaging/DEPLOY.md` for the maintenance window and the write-loss boundary.

Owner-approved experimental-ledger policy (TRACK-76): all experimental multi-user databases are disposable. This does NOT authorize deleting reviewer evidence, other sessions' fixtures, source, or production data. Actual default/upstream migrations 0001, 0002, hygiene0003, optional limiter0004, repair0005 and guard0006 keep their immutable bytes/checksums. The settings migration was relocated from an old experimental version to `0007_user_settings.sql`, with new owner-aware indexes. The old bytes are preserved as a test fixture. A real preserved review database demonstrated settings003 checksum `1e89bcb129fcdeadb396a48b8310419532c080e8b4a6e7e329532d7fbed72074ac4a43f70bb0a389f16cc885343b865f`; that is incompatible with default's hygiene003, not an alternative spelling. Startup rejects it explicitly before migrations and never edits its ledger. The rejected PR49 head also recorded a `0005_neutral_default_timezone.sql` whose checksum (`401b8424ab749e4d1644fee4ea86f401143869ed8e2f227f46475ae519bf92b5fd59c3106f4be298bc57f92f58d7ae11`) is not the published 0005; startup names that ledger too, because the UTC it already wrote cannot be reconstructed. Migration 0012 uses the next free version instead of reusing 0005.

Recreating an experiment: preserve the old file and branch/evidence; positively identify a database you created for the task (not a production or reviewer file); choose a new empty path in your owned scratch directory and start the current binary with BREADTRACKER_DATABASE_PATH pointing there. This recreates only a disposable experiment, not a data-preserving production upgrade. Never edit `_sqlx_migrations`, delete unrelated files, or point production at an empty path to bypass a mismatch. Any non-disposable ledger mismatch stops for operator intervention. The multi-user stack owns versions 0008 to 0012 (invites, public-authentication admission, disabling, authenticated request budgets, install provenance); a later schema change takes 0013 or above and must not reuse an earlier number. A database that recorded the real limiter0004 still needs separately integrated PR31 migration/runtime; the fixture does not ship that runtime. PR35/36/38 are not inherent schema prerequisites.

Scheduler query cost: 0007 adds `(user_id, remind_time)` with the live-reminder partial predicate, `(user_id, kind, id)` for to-do selection, `(user_id, date, item_id)` for completions and sent markers. Date/todo completion queries use UNION so each arm uses its index and duplicate projections are removed. `server/tests/scheduler_work.rs` executes the actual full projections with 10,000 items, 120,000 completions and 10,000 markers: zero-result work is 21/127/61 VM steps, compared with old experimental full scans of about 90,000/1,370,000 steps. This is not a constant all-history bound: real candidate to-dos and their historical completions still cost work proportional to their matching rows; stats and sync return retained data as before. Any-date to-do suppression, quiet-window original-date release and per-user marker uniqueness remain unchanged.

## Invites and signup

Accounts are created only with an invite code, made by the server owner on the command line: `bread-tracker-server invite create [--ttl 7d]` prints a single-use code (`XXXX-XXXX-XXXX`, about 60 bits; `--ttl` takes `7d`, `48h` or `90m`, default `BREADTRACKER_INVITE_TTL_DAYS`, 7), `invite list` shows each invite's state (active, used by whom, expired, revoked; never the code) and `invite revoke <id>` cancels an unused one. Only the SHA-256 of a code is stored, in `invites`; rows are kept after use as a record.

`POST /signup {invite, name, device_name?, timezone?}` is public. In one transaction it claims the invite, creates the user, applies the phone's timezone to the user's default settings row (an unknown timezone is ignored, so the account stays on UTC) and creates the first device, then returns `{user_id, name, device_id, token}`. The claim is a single `UPDATE ... WHERE used_at IS NULL AND revoked_at IS NULL AND expires_at > now`, the first write of the transaction, so concurrent redemptions are serialised and exactly one wins. If a later step fails, including a taken name, the whole transaction rolls back and the invite stays usable. Names are 1 to 40 letters, digits, spaces, `.`, `_` or `-`, unique ignoring case (409 when taken). A user created this way is never an admin; admins exist only through the migration.

An unknown, used, expired or revoked code all answer the same 401, so a caller cannot tell which codes exist. Pairing and signup share a durable SQLite source budget: five failures per ten minutes, including malformed JSON and invalid names. Admission, validation and redemption hold one `BEGIN IMMEDIATE` transaction. Success does not consume the budget; failed redemptions roll back their effects before failure accounting commits. At 1,024 active source budgets, new sources are rejected before lookup until an expired slot is available; live budgets are never evicted. A blocked source cannot redeem even a valid code. Forwarded addresses are accepted only from explicitly configured `BREADTRACKER_TRUSTED_PROXIES`; deployers must verify the actual transport peer and proxy header sanitization. No deployment topology is inferred from unit tests. Migration 0009 is separate from the reserved PR31 migration 0004; importing a database with recorded 0004 still requires its actual migration/runtime integration.

## Administration and your own devices

Management is command line only; the server exposes no admin routes over HTTP.

- `pair [--user <name>]` prints a one-time code (15 minutes, single use) bound to that user; with no flag it is the admin's, as before. An unknown name is an error, never a fallback to the admin.
- `devices [--user <name>]` lists devices with their owner. `revoke <id>` is unchanged.
- `user list` and `user remove <name> --yes`. Removal needs the explicit flag, refuses admins, and deletes the user and everything they own in one transaction (devices, so their tokens stop working at once, items, completions, settings, reminder markers, pairing codes). It prints what was deleted. Invites keep their record with the user cleared. `users::remove` is the single deletion path; self-service account deletion should reuse it.
- `POST /pair-code` (authenticated) returns `{code, expires_in_secs}`: a 15 minute single-use code that adds another device to the caller's own account, redeemed with `POST /pair` like any other. It is bound to the user from the caller's token. A user keeps at most 3 unexpired codes; asking for another replaces the oldest.
- The first-run pairing code printed at startup depends only on the admin having no device. Other users' devices do not suppress it, and no code is ever printed for another user.

## The app and accounts

The phone's database is one store, shared by whichever account signs in, so the app takes care that one account's rows are never mixed into, or pushed to, another.

- The pairing screen has two modes. **Join with an invite** asks for the server, a display name and the invite code, sends the phone's IANA timezone (`flutter_timezone`; if it cannot be read the account stays on UTC) and calls `POST /signup`. **Pair with a code** adds the phone to an existing account (`POST /pair`), with a code from the CLI or from "Add another device" in Settings (`POST /pair-code`).
- `POST /pair` and `POST /signup` return `user_id` and `name`; `GET /me` returns them for a token. The app records the account (server, user id, name) in preferences and shows "Signed in as NAME on SERVER" in Settings.
- **Switching accounts.** Joining or pairing compares the new account with the recorded one. A different user id or a different server deletes the local items and completions first, resets the sync cursor, and only then syncs, so nothing of the old account can reach the new one. The same account again keeps its data and offers all of it to the server (as before). A phone paired before the app recorded accounts asks `/me` once to learn its owner; if that is impossible (offline, revoked, old server) the owner is unknown and the data is treated as foreign. A phone that was only ever used offline has no history: its data is kept and uploaded to the account it joins.
- Whenever the screen cannot rule out a wipe (the phone has data and a history) it first asks for confirmation, saying that unsynced changes to the old account are lost. Cancel changes nothing.
- **Sign out** revokes the device on the server and forgets the token and cursor. With "keep data" the account is remembered, so signing back in to the same account keeps the data and signing in to another wipes it; "sign out and delete data" also forgets the account.
- Against a server that predates accounts the app reports "too old for this version" rather than guessing.

## Family

A parent can assign chores, habits and to-dos to a linked child's list. The child keeps their own
account, phone and full use of the app with no parent linked; a family link is optional, and only
the child can accept one. The multi-user chain (TRACK-17 to TRACK-20) is integrated on `feature/app`
(see Users and migrations), so family work builds on that foundation and no longer waits for it.

### Links

- One row per pair in `family_links(id, parent_id, child_id, status, default_permission, notify_on_complete, created_at, accepted_at)`, unique on `(parent_id, child_id)`; status is pending, active or revoked, and a user cannot link to themselves. The schema, provenance and override columns belong to TRACK-55; existing items default `created_by` to their owner so nothing changes for current users.
- The child offers a short-lived single-use family code from Settings. The parent redeems it to create a pending link, and the child confirms who is asking before it becomes active. A parent can never link unilaterally, and pending links expire.
- Either side can unlink at any time. Revocation ends the parent's access immediately, including anything the parent's devices have not pulled yet.
- Roles are per link direction, never per account: one child may have several parents, one parent several children, and a user may be both a parent in one link and a child in another. The child always sees who can assign to them.

### Item provenance and visibility

- A parent-added item is stored under the child's `user_id` with `created_by` set to the parent. The child's normal sync pull delivers it through the same cursor; the child needs no new endpoint.
- `created_by` is immutable provenance that the server sets. It is never trusted from a client payload, on the child's `/sync` or on the parent routes. An item the child creates keeps `created_by` = the child.
- Parent writes use dedicated authenticated routes (list children, create, update and delete an assigned item, read assigned-item progress), never the parent's `/sync`. Parent reads return only items the parent assigned plus completions and XP progress for those. They never return the child's own items, settings, devices, push tokens or full stats.
- The child's app learns `created_by` and a read-only `effective_permission` with each item so it can label assigned items and grey out controls, but the server remains the authority.

### Permission levels

The effective level is the item's override when set, otherwise the link's default. A new link defaults to complete_only. The server computes the level and enforces it on every write.

| Level | The child may write |
| --- | --- |
| `complete_only` | completions, including completion tombstones (an un-tick) |
| `complete_and_reschedule` | the above plus `due_date`, `weekdays`, `remind_time` and `active` |
| `full` | the above plus `name`, `points` and delete |

- A write outside the effective level is refused per row with a reason the app can show and revert. It must not fail the whole batch silently.
- Changing a link's default permission takes effect on the child's next pull and does not rewrite history. The server advances the `seq` of that parent's items under the child, and does not touch their `updated_at`, so the child's cursor-based pull delivers the new effective permission without pretending the item itself was edited. An item override still wins.
- Tightening a level changes future edits only; edits the child already made stay.

### Revocation

When a link is revoked, the items the parent added stay on the child's list as the child's own: `created_by` is cleared to the child, and the child can delete them like any other item. The parent stops seeing those items and the child's progress immediately, and the parent's devices receive nothing more about the child on their next sync.

### XP, levels, streaks and perfect days

- Parent-assigned items earn XP, levels and streaks exactly like the child's own items, and they join perfect-day scheduling. The shared rules in `spec/xp_vectors.json` do not fork.
- Perfect days follow TRACK-65's immutable creation date: an item joins a day's schedule only from the day after it was created, and the server's timestamp, not a client clock, sets that date. A parent adding an item today therefore cannot revoke a perfect day that was already earned.
- The parent sees the child's XP for the items the parent assigned, not the child's total or any XP from the child's own items.

### Completion alerts

- Completion alerts are per link, default off, and controlled by the parent.
- They are batched so a child ticking several items produces one message, respect the parent's own reminders switch and quiet hours, and are suppressed rather than deferred inside quiet hours.
- The payload carries only the name of the assigned item and no tokens. The child's reminders are unchanged and go to the child only.

## Defaults for new users

Nothing of the original owner's setup is offered to anyone else. A new user's timezone is the phone's at signup, or UTC; the app's empty Today screen asks the user to create their own first item, with a secondary "Add a few examples" that adds six generic chores (bins, plants, laundry, bathroom, tidying, dishes) only when tapped. The owner's household list is gone from the app; if the owner wants it back it is a matter of creating those items (a seed or import file was considered and not added, since nothing in the repository should be personal). The pairing screen no longer names a specific host or container. Migration 0001 still seeds Australia/Perth because shipped migrations are not edited; startup selects UTC for the admin only when the database has no application tables before migrations. It never infers freshness from the number of currently paired devices, so established households retain their timezone even after every device is revoked. All applied migration files remain unchanged. The default server URL stays, since a second user uses the same server.

## Limits and abuse protection

A server that strangers can hold accounts on needs ceilings, all configurable with environment variables (a value that is not a positive integer falls back to the default):

| Limit | Default | Variable |
| --- | --- | --- |
| Users on the server | 20 | `BREADTRACKER_MAX_USERS` |
| Items per user (tombstones count) | 2000 | `BREADTRACKER_MAX_ITEMS_PER_USER` |
| Completions per user (tombstones count) | 50000 | `BREADTRACKER_MAX_COMPLETIONS_PER_USER` |
| Devices per user | 10 | `BREADTRACKER_MAX_DEVICES_PER_USER` |
| Authenticated requests per device token per minute | 120 | `BREADTRACKER_REQUESTS_PER_MINUTE` |

- **Row ceilings.** `apply_push` counts the user's rows inside its write transaction, so concurrent requests cannot both slip under the limit. Only new rows count: updating, deleting or re-sending existing rows always works. A batch that would cross a ceiling stores nothing and gets HTTP 422 with a message naming the limit. The per-request batch (5000 rows) and body (2 MiB) limits still apply. Tombstones count and are retained until a safe acknowledgement/resync protocol exists.
- **Completions need an item.** A completion for an item the user does not hold (live or tombstoned, or in the same batch) rejects the entire batch with HTTP 400 and no writes. Own tombstoned references remain compatible with old clients. Dates far in the future or before 2020 were already refused.
- **Devices.** Pairing and `POST /pair-code` stop at the device limit (422). A pairing code is only consumed when the device is created, so a refused code is not wasted.
- **Users.** Signup stops at the user cap (422 "not accepting new accounts") and rolls the invite claim back, so the invite is still valid later. This is not counted as a bad-invite failure for the rate limit.
- **Request rate.** Every authenticated request counts against its device token in a SQLite-backed one-minute window shared by processes and retained across restart; over the limit gets 429. Public pairing and signup share the durable per-source budget described above. Expired device windows are swept and revocation cascades their rows. The app distinguishes reachable 4xx policy refusals from network/offline errors and keeps local changes.
- **Disabling.** `user disable <name>` and `user enable <name>` toggle `users.disabled_at`. A disabled user's devices get 403 on every authenticated route, the scheduler skips them, and nothing is deleted. Admins cannot be disabled.
- **Logging.** Each authenticated request logs the user id, method, path and status (never the token, the query string or any code), and limit hits log the user id, so abuse can be traced to an account.

## Account deletion, export and what the owner can see

- `DELETE /account {confirm}` deletes the caller's account and everything they own in one transaction, through the same `users::remove` as the CLI's `user remove`. `confirm` must equal the account name (case and surrounding spaces ignored), otherwise 400 and nothing is deleted. Every device of the account stops working at once, unredeemed pairing codes die with it, and the name becomes free. Admin accounts get 403: the server owner removes (or keeps) their own account on the server.
- `GET /export` reads one SQLite transaction snapshot and returns one JSON document with the caller's account, settings, device names and times, items and completions, tombstones included (marked by `deleted_at`). It never contains tokens, token hashes or push tokens, and only the caller's rows. The `items` and `completions` arrays use the sync wire format, so they can be pushed back to an account unchanged. The CLI's `user export <name>` prints the same document, built by the same function.
- In the app, Settings has **Export my data** (copies small JSON exports to the clipboard and shares larger exports as a complete JSON file through the platform share sheet) and **Delete my account** (type the account name to enable the button; the phone's token, cursor, recorded account and local items and ticks are removed only after the server confirms, so a refusal or an unreachable server changes nothing).
- Deleting an account does not touch backups. The Litestream replica and any backup the owner took still hold the data until they rotate out (retention is 168 h by default in `packaging/litestream.yml`).

**What the server owner can see.** The server stores users' items, completions, settings, device names and push tokens in plain SQLite, with no per-user encryption, and the owner administers the host and its backups. They can read every user's data and export it with `user export`. Users are not anonymous to them: names are chosen at signup. Tokens and invite codes are stored only as hashes. This is said plainly to anyone invited: on the join screen and in `docs/ONBOARDING.md`.

## CI

`.forgejo/workflows/check.yml` runs on pushes to `feature/**`, `fix/**` and `main`, on the self-hosted runner: server `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` and `cargo test --workspace --locked`, app `flutter analyze` and `flutter test`, a separate `xp-vectors` job that validates `spec/xp_vectors.json` and runs the server and app rules tests against it side by side, and a `docker build` of `packaging/Containerfile` (built, not pushed). It does not run on pull requests, because the repo is public and the runner is self-hosted. The Flutter jobs need the toolchain at `~/.local/flutter` on the runner (see `scripts/android-env.sh`); they do not need `google-services.json` (only an APK build does, see the Android build notes in app/NOTIFICATIONS.md).
