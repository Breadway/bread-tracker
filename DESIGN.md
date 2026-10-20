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

A scheduler ticks every 30 s in the configured timezone (default Australia/Perth). An item is due once its local `remind_time` has passed (chores and habits on their weekdays, to-dos on their due date) and stays eligible for 6 hours (`BREADTRACKER_REMINDER_GRACE_MINS`) so a restart does not blast old reminders. It is skipped when already completed today (a to-do counts as done if it was completed on any day) or already sent (`reminders_sent`). Nothing is sent during quiet hours; a reminder whose time falls inside the quiet window is held and released when the window ends, and then has its full grace window from that moment (including the next morning for a window that wraps midnight). Quiet hours need both a start and an end: `PUT /settings` rejects setting only one. Nothing is marked sent when no device has an FCM token, so a reminder can still go out after a device registers. Dead tokens reported by FCM (UNREGISTERED) are cleared.

Messages are FCM HTTP v1, Android channel `reminders`, high priority, with data `item_id`, `date`, `kind`. There are two delivery modes, chosen by `BREADTRACKER_PUSH_MODE`:

- `notification` (default): the message has a notification block, so Android shows it by itself even when the app is not running. It cannot carry action buttons. Tapping it opens the app on the Today screen, on the reminded day. While the app is open in the foreground the app shows the notification itself, with the Mark done action.
- `data`: the message has data only (title and body are sent as data). The app's background handler builds the notification, so every reminder, including when the app is closed, has a **Mark done** action. This depends on Android running the app's background handler for a high priority data message, which can be delayed or skipped by Doze, battery optimisation and force stop, so it is opt in.

**Mark done** runs in a background isolate without opening the app. It writes the same dirty completion row that ticking the item would (nothing if the item is deleted or already done; a to-do counts as done if completed on any day), then tries a sync with a 25 s limit. If the phone is offline the row stays dirty and syncs when the app next runs. The app re-reads the database when it returns to the foreground, because the background isolate writes to the same file.

Reminders fire once per item per day. Repeat, overdue to-do reminders and snooze are not implemented: they need new server state (a reminder count or snooze time per item and day) and a decision on how persistent reminders should be, so they are left for a separate change.
