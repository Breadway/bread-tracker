# Bread Tracker design

Separate from the Nextcloud Chores calendar and the Gotify cron reminders. Server and
database live on a server you host; the Android app (Flutter) is offline-first and syncs to it.

## Rules (shared by server `server/src/xp.rs` and the app; vectors in `spec/xp_vectors.json`)

- XP per completion is the item's `points` (defaults: chore 10, habit 15, to-do 20). A date counts once per item.
- A **perfect day** is a date with at least one scheduled chore or habit where every scheduled chore and habit is done. It adds 50 XP. To-dos are never scheduled, so they never make or break a perfect day. Scheduled means active and the date's weekday bit is set (`weekdays` bit0 = Monday .. bit6 = Sunday).
- 120 XP per level; level = xp / 120 + 1. Ranks, one per level and clamped at the last: Flour, Dough, Proofing, Rising, Baked, Golden Crust, Sourdough Master.
- A habit **streak** is the run of consecutive completed days ending today, or ending yesterday when today is not done yet.
- Un-ticked completions and completions of deleted items earn nothing.
- Perfect days are evaluated against the current item definitions, not the schedule as it was in the past.

## Sync

- Rows: `items` and `completions` (key `item_id` + `date`). Every row has `updated_at` (client unix ms) and `deleted_at` (tombstone, so an un-tick or delete syncs).
- Server assigns a monotonic `seq` to every accepted write. The cursor is a `seq`, so clock skew cannot lose rows.
- `POST /sync {since, items, completions}` pushes local changes then returns every row with `seq > since` plus the new `cursor`, in one round trip. `GET /sync?since=` is pull only.
- Conflicts: last write wins by `updated_at`; ties keep the server row. Timestamps more than a day in the future are clamped.
- Batches are limited to 5000 rows and 2 MiB. A completion dated more than 7 days ahead of the server's UTC date, or before 2020, is rejected; `completed_at` is clamped like `updated_at`.
- Housekeeping runs hourly in the scheduler: `reminders_sent` older than two days and expired pairing codes are deleted. Tombstoned items and completions are retained because the current cursor protocol has no per-device acknowledgement or forced-full-resync contract; pruning them could strand an offline client. XP and streak statistics use all retained history. A bounded-history or retention policy requires an explicit backward-compatible protocol/product decision before implementation.

## Auth

Single user. `bread-tracker-server pair` prints a one-time code (valid 15 minutes, stored hashed). `POST /pair {code, device_name}` returns a bearer token once; only its SHA-256 is stored. Pairing is limited to 5 attempts per 10 minutes. All other routes except `/healthz` need `Authorization: Bearer <token>`.

## Reminders

A scheduler ticks every 30 s in the configured timezone (default Australia/Perth). The server uses one timezone for "today", reminder times and quiet hours; the app stamps completions with the phone's own calendar date, so the two only agree when the phone is in the server's zone (the intended single-timezone assumption). The app's Settings has a Reminders section that reads and writes `GET/PUT /settings` (reminders on/off, quiet hours, timezone) and shows a warning when the server's current UTC offset (`utc_offset_minutes` in the settings response) differs from the phone's. It does not change the server zone on its own. An item is due once its local `remind_time` has passed (chores and habits on their weekdays, to-dos on their due date) and stays eligible for 6 hours (`BREADTRACKER_REMINDER_GRACE_MINS`) so a restart does not blast old reminders. It is skipped when already completed today (a to-do counts as done if it was completed on any day), already sent (`reminders_sent`), or inside quiet hours. A reminder whose time falls inside the quiet window is held and released when the window ends, and then has its full grace window from that moment (including the next morning for a window that wraps midnight). Quiet hours need both a start and an end: `PUT /settings` rejects setting only one. Nothing is marked sent when no device has an FCM token, so a reminder can still go out after a device registers. Dead tokens reported by FCM (UNREGISTERED) are cleared.

Messages are FCM HTTP v1 with notification + data (`item_id`, `date`, `kind`), Android channel `reminders`, high priority. The app's "Mark done" action uses the data to write the completion.

## Users and migrations

Every owned row belongs to a user: `items`, `completions`, `devices`, `reminders_sent` and `pairing_codes` carry a `user_id` referencing `users(id)` with `ON DELETE CASCADE`, so removing a user removes only their data. Keys are per user: items are `(user_id, id)`, completions `(user_id, item_id, date)`, reminders `(user_id, item_id, date)`. A client pushing an item id that another user also holds therefore creates or updates its own row and cannot touch the other one. Device token hashes stay globally unique. `meta.seq` remains one global counter; each user's own `seq` values have gaps, which is harmless because a cursor only means "everything larger". Sync reads search the `(user_id, seq)` indexes (`store::pull_query_plans` and a test prove it).

**Not multi-user yet.** The schema is ready, the routes are not: every request is still attributed to the first user (`users::ADMIN_ID`), `GET/POST /sync` and `/stats` read only that user's rows, and only that user's devices can authenticate (a device owned by anyone else gets 401). The background reminder scheduler is single owner as well: items, completions, sent markers and destination devices are all the first user's, so a reminder can never carry one user's item to another user's device and another user's completion cannot suppress it. Other users get no scheduled work at all, which fails closed (`store::reminder_*`, `push::tick`, tested with two owners and two devices). `settings` is still one global table. Do not create a second user's credentials in a deployment until the scoping and per-user settings changes (TRACK-18, TRACK-19) are integrated and reviewed.

User names are 1 to 40 characters: Unicode letters and digits, space, `.`, `_` and `-`, no leading or trailing space. Two names are the same name when their caseless keys are equal (`users::name_key`: canonical decomposition plus Unicode default case folding), so `Älex` and `älex` collide like `Alex` and `alex`. Creation (`users::create`), rename (`users::rename`), lookup (`users::find_by_name`) and the admin naming path all use that key, and a unique index on `users.name_key` enforces it in the database.

SQLite cannot case fold Unicode, so plain SQL has limits. Raw inserts and renames to printable ASCII names keep the key in step (a trigger sets plain lower case). A raw insert or rename to a non-ASCII name is refused unless the key changes with it (migration 0006), because a stale key would hide the user from lookup and let a duplicate in. To rename with SQL while the server is stopped, set `name_key = NULL` together with the new name. At every start, before the listener and the scheduler, `users::reconcile_name_keys` recomputes every user's key from their name inside one `BEGIN IMMEDIATE` transaction: missing and stale keys are repaired, and if two users would share a key nothing is changed and startup fails naming both, never by overwriting a user. SQLite's `NOCASE` is ASCII only and is no longer relied on.

The first user starts as `admin` (migration 0002) and is named once from `BREADTRACKER_ADMIN_NAME`. "Once" is recorded in `users.name_confirmed`, not inferred from the text, so choosing the name `Admin` or `admin` is final too. Restarting with the same or another value changes nothing, and a name that another user already holds stops startup with a clear message instead of renaming anything.

### Migration ledger and numbering

SQLx identifies a migration by its numeric version and the SHA-384 of its file, records it once, and applies any version it has not recorded in version order. It never reruns a recorded version and refuses to start if a recorded file changed or is missing. Rules for this repository: never edit a migration that has been applied anywhere, versions are unique numbers (a suffix does not make `0003_a` and `0003_b` different), and a database change that a recorded version cannot deliver is a new, later, additive version.

| Version | File | Where | Purpose |
|---|---|---|---|
| 0001 | `0001_init.sql` | default | initial schema |
| 0002 | `0002_users.sql` | this PR | users, `user_id` and per-user keys; rebuilds five tables |
| 0003 | `0003_hygiene_indexes.sql` | default (TRACK-8) | scheduler and to-do suppression indexes |
| 0004 | `0004_pair_rate_limits.sql` | PR31 (TRACK-5), reserved | durable pairing failure budgets |
| 0005 | `0005_users_forward_repair.sql` | this PR, published at 6f9986e (treat as applied) | restores 0003's indexes, name key, durable admin naming |
| 0006 | `0006_users_name_key_guard.sql` | this PR | replaces 0005's rename trigger so a non-ASCII raw rename cannot leave a stale key |
| 0007 and up | | downstream, assigned after the applied-ledger inventory below | per-user settings (PR45), invites (PR46), TRACK-65 and later |

Why 0005 exists. 0003 was applied on deployments before 0002 can run, and 0002 rebuilds `items` and `completions`, which drops every index on them. SQLx will not rerun the recorded 0003, so those indexes would silently disappear and the scheduler would scan the whole history again. 0005 recreates them with `IF NOT EXISTS`, so it is a no-op wherever they survived (a fresh database, where 0002 runs before 0003). It also adds `completions_item_date (item_id, date)`, because the primary key `(user_id, item_id, date)` cannot serve the to-do suppression lookup by item id.

Ledgers this set upgrades, each proven on a populated file database in `server/tests/ledger.rs`: 0001 only; 0001+0003; 0001+0003+0004; 0001+0004; 0001+0002 (the earlier PR43 head); 0001+0002+0003; and a fresh database. Also proven there: published checksums of 0001, 0003 and 0004 are unchanged, a failure inside 0002 rolls back to the untouched legacy database and a retry succeeds, a failure inside 0005 keeps the 0002 result and a retry completes it, an edited recorded migration is refused, and this branch without PR31's 0004 refuses a database that already recorded 0004 rather than guessing.

Failure boundaries. Every migration commits in its own transaction, so a failure rolls back only that migration. If 0005 or 0006 fails, 0002 stays committed (`ledger.rs` proves it): the server refuses to start, the database is a valid intermediate state, and starting again after fixing the cause completes the rest. It is not "the original database": returning to the previous release means restoring a full pre-upgrade copy, taken with all writers stopped. See the runbook in `packaging/DEPLOY.md` for the maintenance window and the write-loss boundary.

Downstream numbering (TRACK-76) needs an inventory, not a blanket renumber. The downstream branches carry `0003_user_settings.sql`, `0004_invites.sql`, `0005_neutral_default_timezone.sql` and `0006_user_disabled.sql`. Whether a version may be renamed depends on whether any database has already recorded it. Before changing any of them: (1) list, for every downstream branch, which version numbers and checksums have actually been applied to a database (scratch bases, review databases, anything a person ran), (2) renumber only files proven never applied, in coordinated branches, to the next free version after the highest one integrated here (0007 onward), keeping the contents, (3) never edit or delete rows of `_sqlx_migrations` and never change a published file to make a ledger agree, (4) for a database that already recorded a colliding 0003 or 0004, keep the original database and branch, and provide a supported export or an explicitly consented recreation path, with a test. PR31's `0004` is needed here only when a target database has already recorded `0004` or when its limiter policy ships; this PR refuses such a database (`ledger.rs`) rather than guessing. PR35, PR36 and PR38 are independent features and are not prerequisites of this schema-only change. Nothing downstream is renumbered in this PR.

## CI

`.forgejo/workflows/check.yml` runs on pushes to `feature/**`, `fix/**` and `main`, on the self-hosted runner: server `cargo clippy -D warnings` and `cargo test --locked` (plus `cargo fmt --check`, report only until the code base is formatted once), app `flutter analyze` and `flutter test`, a separate `xp-vectors` job that validates `spec/xp_vectors.json` and runs the server and app rules tests against it side by side, and a `docker build` of `packaging/Containerfile` (built, not pushed). It does not run on pull requests, because the repo is public and the runner is self-hosted. The Flutter jobs need the toolchain at `~/.local/flutter` on the runner (see `scripts/android-env.sh`); they do not need `google-services.json` (only an APK build does, see the Android build notes in app/NOTIFICATIONS.md).
