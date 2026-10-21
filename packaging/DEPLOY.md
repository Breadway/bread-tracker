# Deploying bread-tracker on Hestia

```sh
rsync -a --delete --exclude target --exclude .git ./ hestia:~/bread-tracker/
ssh hestia 'cd ~/bread-tracker && mkdir -p /var/backups/bread-tracker 2>/dev/null; \
  docker compose -f packaging/compose.yaml --project-directory . up -d --build'
curl -s http://127.0.0.1:7290/healthz            # on Hestia: ok
docker exec bread-tracker bread-tracker-server pair   # one-time pairing code for the phone
```

Run compose as the `breadway` user, not root.

## Devices

```sh
docker exec bread-tracker bread-tracker-server devices          # id, name, last seen, push yes/no
docker exec bread-tracker bread-tracker-server revoke 1a2b3c4d  # id or unique prefix (4+ chars)
```

A lost or sold phone: revoke it here. Its token stops working immediately and it is no longer sent reminders.

## Edge

Hestia's own Caddy (LAN) and Aegis (public) both reverse-proxy `tracker.breadway.dev` to port 7290; the blocks live in the infra repo (`hestia/Caddyfile`, `aegis/Caddyfile`). Aegis reaches Hestia over Tailscale, so the container must bind an address Tailscale can reach: set `BREADTRACKER_BIND=0.0.0.0:7290` in `.env` (as breadboard does with 7280) or the Tailscale IP.

Rate limiting is per client address. Behind a reverse proxy the server only sees the proxy's address unless it is told to trust it. Set `BREADTRACKER_TRUSTED_PROXIES` to the address or CIDR the proxy connects from (comma separated, for example `100.64.0.0/10` for Tailscale) so `X-Forwarded-For` is used. Leave it empty and all callers share one bucket, which still only counts failed attempts. Check from the container that the address Aegis connects from matches before relying on it, and make sure the proxy overwrites rather than appends client-supplied `X-Forwarded-For`.

The compose file passes `BREADTRACKER_TRUSTED_PROXIES` through from `.env`.

## Push (FCM)

Put the Firebase service-account JSON on Hestia outside the repo (mode 600) and set in `.env`:

```
BREADTRACKER_FCM_KEY_FILE=/home/breadway/secrets/bread-tracker-fcm.json
BREADTRACKER_FCM_SERVICE_ACCOUNT=/run/secrets/fcm.json
```

Without it the server runs with push disabled (logged at startup).

Optional: `BREADTRACKER_PUSH_MODE=data` makes reminders data-only so the app can add a Mark done button to every one (default `notification`). Read app/NOTIFICATIONS.md and test on a phone before turning it on. The compose file passes it through from `.env`.

## Backup and restore

Litestream writes a file replica to `BREADTRACKER_BACKUP_DIR`. Restore into a scratch file to verify:

```sh
docker run --rm -v /var/backups/bread-tracker:/b -v /tmp:/o litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b/litestream
sqlite3 /tmp/restored.db 'select count(*) from items;'
```

## Upgrading an existing single-user deployment to multi-user

The multi-user release adds migrations that run automatically at server start: `0002_users.sql` adds a `users` table, gives all existing data to the first user (id 1, the admin) and rebuilds `items`, `completions`, `devices`, `reminders_sent` and `pairing_codes` with a `user_id` and per-user keys; `0003` moves the global settings to the admin; `0004` adds invites; `0005` and `0006` are small. They run in one transaction each, so a failure leaves the previous schema and the server refuses to start. They are not reversible by starting an older image, so back up first.

1. **Check the Litestream replica** is current and restorable, and note the counts:
   `packaging/restore-test.sh` (prints the item and device counts).
2. **Take a copy you control.** The runtime image has no `sqlite3`, so copy the volume's files (database and WAL together) while the server is stopped, which is the simplest way to get a consistent copy:

   ```sh
   docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker litestream
   docker run --rm --user root -v bread-tracker_bread-tracker-data:/d:ro -v "$HOME/bread-tracker-pre-multiuser:/b" \
     --entrypoint cp bread-tracker:local -a /d/. /b/
   ```

   Create `$HOME/bread-tracker-pre-multiuser` first and keep it until you are happy with the result. (The volume name is `<compose project>_bread-tracker-data`; check with `docker volume ls`.)
3. **Name the first user.** Set `BREADTRACKER_ADMIN_NAME` in `.env` (for example `Riley`). It is applied once: it renames the user only while it still has the placeholder name `admin`, so changing it later does nothing. Unset, the user is called `admin`.
4. **Deploy** as in the first section.
5. **Check.** `docker exec bread-tracker bread-tracker-server user list` shows one admin with your devices; the item and device counts match step 1 (`devices` lists them). Existing phones keep working without an app update, because the sync API only gained fields. The timezone and quiet hours you had are now the admin's own settings.

To roll back, stop the stack, restore the files from step 2 into the volume and start the previous image.

## Invites

Create a code for a new user (valid 7 days by default, single use) and send it to them:

```sh
docker exec bread-tracker bread-tracker-server invite create            # or --ttl 48h
docker exec bread-tracker bread-tracker-server invite list
docker exec bread-tracker bread-tracker-server invite revoke <id>
```

The code is shown once. Signup is a public endpoint (`POST /signup`) behind the same reverse proxy as the rest: set `BREADTRACKER_TRUSTED_PROXIES` to your proxy's address range so the per-address rate limit sees the real client rather than the proxy. `BREADTRACKER_INVITE_TTL_DAYS` changes the default lifetime.

## Users and devices

```sh
docker exec bread-tracker bread-tracker-server user list
docker exec bread-tracker bread-tracker-server devices [--user NAME]
docker exec bread-tracker bread-tracker-server pair [--user NAME]    # a code for that user's new device
docker exec bread-tracker bread-tracker-server revoke <device id>
docker exec bread-tracker bread-tracker-server user export NAME     # everything stored for a user, as JSON
docker exec bread-tracker bread-tracker-server user remove NAME --yes # deletes the user and all their data
```

Without `--user`, `pair` makes a code for the admin. Users add their own second device from the app (Settings), which calls `POST /pair-code`; you do not need to be involved. `user remove` cannot be undone, so take a backup first if in doubt; it refuses to remove an admin.

## Limits

The defaults suit a small shared server (20 users, 2000 items and 50000 completions each, 10 devices each, 120 requests a minute per device). Change them in `.env` with `BREADTRACKER_MAX_USERS`, `BREADTRACKER_MAX_ITEMS_PER_USER`, `BREADTRACKER_MAX_COMPLETIONS_PER_USER`, `BREADTRACKER_MAX_DEVICES_PER_USER` and `BREADTRACKER_REQUESTS_PER_MINUTE`. The existing deployment is well below them. To stop someone without deleting their data: `user disable NAME` (and `user enable NAME`).
