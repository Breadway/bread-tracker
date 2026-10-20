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
- Batches are limited to 5000 rows and 2 MiB.

## Auth

Single user. `bread-tracker-server pair` prints a one-time code (valid 15 minutes, stored hashed). `POST /pair {code, device_name}` returns a bearer token once; only its SHA-256 is stored. Pairing is limited to 5 failed attempts per 10 minutes per client address; successful pairings are not counted and one address being blocked does not affect another. The client address is the TCP peer, or the `X-Forwarded-For` client when the peer is listed in `BREADTRACKER_TRUSTED_PROXIES`. All other routes except `/healthz` need `Authorization: Bearer <token>`.

## Reminders

A scheduler ticks every 30 s in the configured timezone (default Australia/Perth). An item is due once its local `remind_time` has passed (chores and habits on their weekdays, to-dos on their due date) and stays eligible for 6 hours (`BREADTRACKER_REMINDER_GRACE_MINS`) so a restart does not blast old reminders. It is skipped when already completed today, already sent (`reminders_sent`), or inside quiet hours. Nothing is marked sent when no device has an FCM token, so a reminder can still go out after a device registers. Dead tokens reported by FCM (UNREGISTERED) are cleared.

Messages are FCM HTTP v1 with notification + data (`item_id`, `date`, `kind`), Android channel `reminders`, high priority. The app's "Mark done" action uses the data to write the completion.
