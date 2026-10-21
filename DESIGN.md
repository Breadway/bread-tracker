# Bread Tracker design

Separate from the Nextcloud Chores calendar and the Gotify cron reminders. Server and
database live on Hestia; the Android app (Flutter) is offline-first and syncs to it.

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
- **Epoch.** The server stores a random `epoch` in `meta` the first time a database is opened and returns it in every sync response. The app remembers it next to the cursor. If it changes (the server was rebuilt or replaced), or the server's cursor is lower than the app's (the database was restored from an older replica, which keeps the same epoch), the app resets its cursor to 0, marks every local row dirty and syncs again, so a fresh or older server gets the phone's data back. Pairing does the same reset. Last write wins by `updated_at` makes the full push safe. A request whose `since` is ahead of the database is answered as if `since` were 0. A server without an epoch (older version) is treated as unchanged.
- Conflicts: last write wins by `updated_at`; ties keep the server row. Timestamps more than a day in the future are clamped.
- Batches are limited to 5000 rows and 2 MiB. A completion dated more than 7 days ahead of the server's UTC date, or before 2020, is rejected; `completed_at` is clamped like `updated_at`.
- Housekeeping runs hourly in the scheduler: `reminders_sent` older than two days and expired pairing codes are deleted, and tombstoned items and completions are deleted for good after `BREADTRACKER_TOMBSTONE_DAYS` (default 180). A device that stays offline longer than that and still holds a row deleted elsewhere would keep it.

## Auth

Single user. `bread-tracker-server pair` prints a one-time code (valid 15 minutes, stored hashed). `POST /pair {code, device_name}` returns a bearer token once; only its SHA-256 is stored. Pairing is limited to 5 failed attempts per 10 minutes per client address; successful pairings are not counted and one address being blocked does not affect another. The client address is the TCP peer, or the `X-Forwarded-For` client when the peer is listed in `BREADTRACKER_TRUSTED_PROXIES`. All other routes except `/healthz` need `Authorization: Bearer <token>`.

Devices can be listed and revoked: `bread-tracker-server devices` prints id, name, pairing and last-seen time and whether push is registered; `bread-tracker-server revoke <id>` deletes a device by id or unique prefix (at least 4 characters). A revoked token gets 401 on its next request. The app has a Sign out action in Settings that calls `DELETE /device` (revokes only the calling device), forgets the token and cursor, and optionally deletes local data. When any request gets 401 the app forgets the dead token and shows the pairing screen again, keeping local data. Tokens still do not expire or rotate.

## Reminders

A scheduler ticks every 30 s and evaluates each user in that user's own timezone (default UTC for new users; the existing data kept its Australia/Perth setting). Each user's "today", reminder times and quiet hours use their zone; the app stamps completions with the phone's own calendar date, so the two only agree when the phone is in the user's configured zone. The app's Settings has a Reminders section that reads and writes `GET/PUT /settings` (reminders on/off, quiet hours, timezone) and shows a warning when the server's current UTC offset (`utc_offset_minutes` in the settings response) differs from the phone's. It does not change the server zone on its own. An item is due once its local `remind_time` has passed (chores and habits on their weekdays, to-dos on their due date) and stays eligible for 6 hours (`BREADTRACKER_REMINDER_GRACE_MINS`) so a restart does not blast old reminders. It is skipped when already completed today (a to-do counts as done if it was completed on any day) or already sent (`reminders_sent`). Nothing is sent during quiet hours; a reminder whose time falls inside the quiet window is held and released when the window ends, and then has its full grace window from that moment (including the next morning for a window that wraps midnight). Quiet hours need both a start and an end: `PUT /settings` rejects setting only one. Nothing is marked sent when no device has an FCM token, so a reminder can still go out after a device registers. Dead tokens reported by FCM (UNREGISTERED) are cleared.

Messages are FCM HTTP v1, Android channel `reminders`, high priority, with data `item_id`, `date`, `kind`. There are two delivery modes, chosen by `BREADTRACKER_PUSH_MODE`:

- `notification` (default): the message has a notification block, so Android shows it by itself even when the app is not running. It cannot carry action buttons. Tapping it opens the app on the Today screen, on the reminded day. While the app is open in the foreground the app shows the notification itself, with the Mark done action.
- `data`: the message has data only (title and body are sent as data). The app's background handler builds the notification, so every reminder, including when the app is closed, has a **Mark done** action. This depends on Android running the app's background handler for a high priority data message, which can be delayed or skipped by Doze, battery optimisation and force stop, so it is opt in.

**Mark done** runs in a background isolate without opening the app. It writes the same dirty completion row that ticking the item would (nothing if the item is deleted or already done; a to-do counts as done if completed on any day), then tries a sync with a 25 s limit. If the phone is offline the row stays dirty and syncs when the app next runs. The app re-reads the database when it returns to the foreground, because the background isolate writes to the same file.

Reminders fire once per item per day. Repeat, overdue to-do reminders and snooze are not implemented: they need new server state (a reminder count or snooze time per item and day) and a decision on how persistent reminders should be, so they are left for a separate change.

## Users (schema)

Every owned row belongs to a user: `items`, `completions`, `devices`, `reminders_sent` and `pairing_codes` carry a `user_id` referencing `users(id)` with `ON DELETE CASCADE`, so removing a user removes only their data. Keys are per user: items are `(user_id, id)`, completions `(user_id, item_id, date)`, reminders `(user_id, item_id, date)`. A client pushing an item id that another user also holds therefore creates or updates its own row and cannot touch the other one. Device token hashes stay globally unique. `meta.seq` remains one global counter; each user's own `seq` values have gaps, which is harmless because a cursor only means "everything larger". Sync reads use the `(user_id, seq)` indexes.

Migration `0002_users.sql` creates the first user (id 1, the admin) and gives it all existing rows. The user is named `admin` by the migration and renamed from `BREADTRACKER_ADMIN_NAME` at start, once, only while it still has that placeholder name. User names are 1 to 40 letters, digits, spaces, `.`, `_` or `-`, unique ignoring case. The `settings` table is not migrated yet.

### Scoping

`require_auth` looks the bearer token up in `devices` and puts both a `DeviceId` and a `UserId` into the request extensions. The user id always comes from that row, never from the request body or query. All queries on `items`, `completions` and `reminders_sent` live on `store::UserDb`, which holds the user id privately and binds it in every statement, so a handler cannot read or write those tables without naming whose data it is. `/sync` (both verbs), `/stats`, `/device` (set token and sign out) and `/push/test` act only on the caller. The sync cursor is the global counter, so it advances when other users write; their rows are never returned and the gaps are harmless.

An FCM token identifies one app install, so it still belongs to exactly one device across all users: registering a token that another device holds clears the token on the old device (only that column) and records it in the log. This is what lets a phone sign in as a different account.

The scheduler loops over users. Each user's reminders are built from their own items, completions and sent markers and go only to their own devices; a failure for one user is logged and does not stop the others, and a user with no registered device does not use up anyone else's reminder.

### Settings

Settings are per user, in `user_settings(user_id, timezone, quiet_start, quiet_end, reminders_enabled)`. Migration 0003 moves the old global values to the admin and drops the global `settings` table. A trigger gives every new user a row with the defaults: UTC, no quiet hours, reminders on (the signup flow may set the phone's timezone on that row). `GET/PUT /settings` and `/stats` use the caller's row, so "today" in `/stats` follows the caller's timezone. `PUT /settings` validates the whole result before writing it (timezone known, times `HH:MM` or empty, quiet hours both set or both empty), so a rejected request changes nothing. The scheduler reads each user's own timezone, quiet hours, grace window handling and switch on every tick.

### Invites and signup

Accounts are created only with an invite code, made by the server owner on the command line: `bread-tracker-server invite create [--ttl 7d]` prints a single-use code (`XXXX-XXXX-XXXX`, about 60 bits; `--ttl` takes `7d`, `48h` or `90m`, default `BREADTRACKER_INVITE_TTL_DAYS`, 7), `invite list` shows each invite's state (active, used by whom, expired, revoked; never the code) and `invite revoke <id>` cancels an unused one. Only the SHA-256 of a code is stored, in `invites`; rows are kept after use as a record.

`POST /signup {invite, name, device_name?, timezone?}` is public. In one transaction it claims the invite, creates the user, applies the phone's timezone to the user's default settings row (an unknown timezone is ignored, so the account stays on UTC) and creates the first device, then returns `{user_id, name, device_id, token}`. The claim is a single `UPDATE ... WHERE used_at IS NULL AND revoked_at IS NULL AND expires_at > now`, the first write of the transaction, so concurrent redemptions are serialised and exactly one wins. If a later step fails, including a taken name, the whole transaction rolls back and the invite stays usable. Names are 1 to 40 letters, digits, spaces, `.`, `_` or `-`, unique ignoring case (409 when taken). A user created this way is never an admin; admins exist only through the migration.

An unknown, used, expired or revoked code all answer the same 401, so a caller cannot tell which codes exist. Failed attempts are counted per client address by the same `FailureLimiter` type as `/pair` (10 per 10 minutes, a separate counter, successes do not count, one blocked source does not affect another, `X-Forwarded-For` is trusted only from `BREADTRACKER_TRUSTED_PROXIES`).

### Administration and your own devices

Management is command line only; the server exposes no admin routes over HTTP.

- `pair [--user <name>]` prints a one-time code (15 minutes, single use) bound to that user; with no flag it is the admin's, as before. An unknown name is an error, never a fallback to the admin.
- `devices [--user <name>]` lists devices with their owner. `revoke <id>` is unchanged.
- `user list` and `user remove <name> --yes`. Removal needs the explicit flag, refuses admins, and deletes the user and everything they own in one transaction (devices, so their tokens stop working at once, items, completions, settings, reminder markers, pairing codes). It prints what was deleted. Invites keep their record with the user cleared. `users::remove` is the single deletion path; self-service account deletion should reuse it.
- `POST /pair-code` (authenticated) returns `{code, expires_in_secs}`: a 15 minute single-use code that adds another device to the caller's own account, redeemed with `POST /pair` like any other. It is bound to the user from the caller's token. A user keeps at most 3 unexpired codes; asking for another replaces the oldest.
- The first-run pairing code printed at startup depends only on the admin having no device. Other users' devices do not suppress it, and no code is ever printed for another user.

### The app and accounts

The phone's database is one store, shared by whichever account signs in, so the app takes care that one account's rows are never mixed into, or pushed to, another.

- The pairing screen has two modes. **Join with an invite** asks for the server, a display name and the invite code, sends the phone's IANA timezone (`flutter_timezone`; if it cannot be read the account stays on UTC) and calls `POST /signup`. **Pair with a code** adds the phone to an existing account (`POST /pair`), with a code from the CLI or from "Add another device" in Settings (`POST /pair-code`).
- `POST /pair` and `POST /signup` return `user_id` and `name`; `GET /me` returns them for a token. The app records the account (server, user id, name) in preferences and shows "Signed in as NAME on SERVER" in Settings.
- **Switching accounts.** Joining or pairing compares the new account with the recorded one. A different user id or a different server deletes the local items and completions first, resets the cursor and epoch, and only then syncs, so nothing of the old account can reach the new one. The same account again keeps its data and offers all of it to the server (as before). A phone paired before the app recorded accounts asks `/me` once to learn its owner; if that is impossible (offline, revoked, old server) the owner is unknown and the data is treated as foreign. A phone that was only ever used offline has no history: its data is kept and uploaded to the account it joins.
- Whenever the screen cannot rule out a wipe (the phone has data and a history) it first asks for confirmation, saying that unsynced changes to the old account are lost. Cancel changes nothing.
- **Sign out** revokes the device on the server and forgets the token and cursor. With "keep data" the account is remembered, so signing back in to the same account keeps the data and signing in to another wipes it; "sign out and delete data" also forgets the account.
- Against a server that predates accounts the app reports "too old for this version" rather than guessing.

### Defaults for new users

Nothing of the original owner's setup is offered to anyone else. A new user's timezone is the phone's at signup, or UTC; the app's empty Today screen asks the user to create their own first item, with a secondary "Add a few examples" that adds six generic chores (bins, plants, laundry, bathroom, tidying, dishes) only when tapped. The owner's household list is gone from the app; if the owner wants it back it is a matter of creating those items (a seed or import file was considered and not added, since nothing in the repository should be personal). The pairing screen no longer names a specific host or container. Migration 0001 still seeds Australia/Perth because shipped migrations are not edited; migration 0005 resets the admin to UTC only on a database that has never had a device (a fresh installation), so a deployment in use keeps its timezone. The default server URL stays, since a second user uses the same server.

### Limits and abuse protection

A server that strangers can hold accounts on needs ceilings, all configurable with environment variables (a value that is not a positive integer falls back to the default):

| Limit | Default | Variable |
| --- | --- | --- |
| Users on the server | 20 | `BREADTRACKER_MAX_USERS` |
| Items per user (tombstones count) | 2000 | `BREADTRACKER_MAX_ITEMS_PER_USER` |
| Completions per user (tombstones count) | 50000 | `BREADTRACKER_MAX_COMPLETIONS_PER_USER` |
| Devices per user | 10 | `BREADTRACKER_MAX_DEVICES_PER_USER` |
| Authenticated requests per device token per minute | 120 | `BREADTRACKER_REQUESTS_PER_MINUTE` |

- **Row ceilings.** `apply_push` counts the user's rows inside its write transaction, so concurrent requests cannot both slip under the limit. Only new rows count: updating, deleting or re-sending existing rows always works. A batch that would cross a ceiling stores nothing and gets HTTP 422 with a message naming the limit. The per-request batch (5000 rows) and body (2 MiB) limits still apply. Tombstones count because they take space until pruned.
- **Completions need an item.** A completion for an item the user does not hold (not stored, not in the same batch) is not stored. It is skipped rather than rejected, so a phone holding such a row cannot be stuck retrying a batch that never succeeds. Dates far in the future or before 2020 were already refused.
- **Devices.** Pairing and `POST /pair-code` stop at the device limit (422). A pairing code is only consumed when the device is created, so a refused code is not wasted.
- **Users.** Signup stops at the user cap (422 "not accepting new accounts") and rolls the invite claim back, so the invite is still valid later. This is not counted as a bad-invite failure for the rate limit.
- **Request rate.** Every authenticated request counts against its device token in a fixed one-minute window held in memory (resets on restart); over the limit gets 429. Public routes are limited per address as before.
- **Disabling.** `user disable <name>` and `user enable <name>` toggle `users.disabled_at`. A disabled user's devices get 403 on every authenticated route, the scheduler skips them, and nothing is deleted. Admins cannot be disabled.
- **Logging.** Each authenticated request logs the user id, method, path and status (never the token, the query string or any code), and limit hits log the user id, so abuse can be traced to an account.
