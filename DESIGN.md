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
- Conflicts: last write wins by `updated_at`; ties keep the server row. Timestamps more than a day in the future are clamped.
- Batches are limited to 5000 rows and 2 MiB. A completion dated more than 7 days ahead of the server's UTC date, or before 2020, is rejected; `completed_at` is clamped like `updated_at`.
- Housekeeping runs hourly in the scheduler: `reminders_sent` older than two days and expired pairing codes are deleted, and tombstoned items and completions are deleted for good after `BREADTRACKER_TOMBSTONE_DAYS` (default 180). A device that stays offline longer than that and still holds a row deleted elsewhere would keep it.

## Auth

Single user. `bread-tracker-server pair` prints a one-time code (valid 15 minutes, stored hashed). `POST /pair {code, device_name}` returns a bearer token once; only its SHA-256 is stored. Pairing is limited to 5 attempts per 10 minutes. All other routes except `/healthz` need `Authorization: Bearer <token>`.

## Reminders

A scheduler ticks every 30 s in the configured timezone (default Australia/Perth). An item is due once its local `remind_time` has passed (chores and habits on their weekdays, to-dos on their due date) and stays eligible for 6 hours (`BREADTRACKER_REMINDER_GRACE_MINS`) so a restart does not blast old reminders. It is skipped when already completed today (a to-do counts as done if it was completed on any day) or already sent (`reminders_sent`). Nothing is sent during quiet hours; a reminder whose time falls inside the quiet window is held and released when the window ends, and then has its full grace window from that moment (including the next morning for a window that wraps midnight). Quiet hours need both a start and an end: `PUT /settings` rejects setting only one. Nothing is marked sent when no device has an FCM token, so a reminder can still go out after a device registers. Dead tokens reported by FCM (UNREGISTERED) are cleared.

Messages are FCM HTTP v1 with notification + data (`item_id`, `date`, `kind`), Android channel `reminders`, high priority. The app's "Mark done" action uses the data to write the completion.

## CI

`.forgejo/workflows/check.yml` runs on pushes to `feature/**`, `fix/**` and `main`, on the self-hosted runner: server `cargo clippy -D warnings` and `cargo test --locked` (plus `cargo fmt --check`, report only until the code base is formatted once), app `flutter analyze` and `flutter test`, a separate `xp-vectors` job that validates `spec/xp_vectors.json` and runs the server and app rules tests against it side by side, and a `docker build` of `packaging/Containerfile` (built, not pushed). It does not run on pull requests, because the repo is public and the runner is self-hosted. The Flutter jobs need the toolchain at `~/.local/flutter` on the runner (see `scripts/android-env.sh`); they do not need `google-services.json` (only an APK build does, see the Android build notes in app/NOTIFICATIONS.md).

