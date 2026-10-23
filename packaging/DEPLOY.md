# Deploying bread-tracker on Hestia

```sh
rsync -a --delete --exclude target --exclude .git ./ hestia:~/bread-tracker/
ssh hestia 'cd ~/bread-tracker && mkdir -p /var/backups/bread-tracker 2>/dev/null; \
  docker compose -f packaging/compose.yaml --project-directory . up -d --build'
curl -s http://127.0.0.1:7290/healthz            # on Hestia: ok
docker compose -f packaging/compose.yaml --project-directory . exec bread-tracker bread-tracker-server pair   # one-time pairing code for the phone
```

Run compose as the `breadway` user, not root.

## Edge

Hestia's own Caddy (LAN) and Aegis (public) both reverse-proxy `tracker.breadway.dev` to port 7290; the blocks live in the infra repo (`hestia/Caddyfile`, `aegis/Caddyfile`). Aegis reaches Hestia over Tailscale, so the container must bind an address Tailscale can reach: set `BREADTRACKER_BIND=0.0.0.0:7290` in `.env` (as breadboard does with 7280) or the Tailscale IP.

## Push (FCM)

Put the Firebase service-account JSON on Hestia outside the repo (mode 600) and set in `.env`:

```
BREADTRACKER_FCM_KEY_FILE=/home/breadway/secrets/bread-tracker-fcm.json
BREADTRACKER_FCM_SERVICE_ACCOUNT=/run/secrets/fcm.json
```

Without it the server runs with push disabled (logged at startup).

## Backup and restore

Litestream writes a file replica to `BREADTRACKER_BACKUP_DIR`. The sync-generation sidecar is deliberately stored outside SQLite, so an older database replica does not roll back the generation clients use to identify the server state. Every database restore must rotate that generation before the server accepts sync requests.

Restore while the server is stopped. Replace the database with the Litestream restore, then run `mark-restored` against the same database path, before starting the service again. For the default Compose setup:

```sh
# Use an isolated copy of the existing data volume for this rehearsal. Never point
# these disposable commands at the live Compose volume or production service.
TASK=track2-restore-check
VOL=${TASK}-data
IMG=${TASK}:local # build this image from the current Containerfile first
set -euo pipefail
docker volume create "$VOL"
docker run --rm --user 0 -v "$VOL:/var/lib/bread-tracker" "$IMG" \
  /bin/sh -c 'printf seeded > /var/lib/bread-tracker/sentinel'
docker run --rm --user 0 -v "$VOL:/var/lib/bread-tracker" -v "$PWD/restored.db:/restore/restored.db:ro" "$IMG" \
  /bin/sh -ec 'sqlite3 /restore/restored.db "PRAGMA integrity_check" | grep -qx ok; sqlite3 /var/lib/bread-tracker/bread-tracker.db "PRAGMA wal_checkpoint(TRUNCATE)"; cp /restore/restored.db /var/lib/bread-tracker/bread-tracker.db'
docker run --rm --user 10001:10001 -v "$VOL:/var/lib/bread-tracker" "$IMG" \
  /usr/local/bin/bread-tracker-server mark-restored
docker run --rm --user 10001:10001 -v "$VOL:/var/lib/bread-tracker" "$IMG" \
  /usr/local/bin/bread-tracker-server
# For restore-failure injection, run a failing replacement command under `sh -ec`
# on a separate disposable volume and assert neither marker nor server is started.
```

Run the same rehearsal with an existing root-owned database/volume and confirm the marker and server can open it as uid 10001. Always use `set -e`; a failed checkpoint, integrity check, or copy must stop before marking or service restart. The marker creates a persistent `.restore-failed` sentinel before making changes and removes it only after successful generation publication. Server startup refuses while the sentinel remains. After any failure, inspect/restore the database, rerun a successful marker, and only then start the service. Checkpoint and close every SQLite user before replacement; never ignore checkpoint errors or copy just the main DB while WAL is live. Verify the restored DB with `PRAGMA integrity_check` before replacement. Do not copy `.sync-generation` or `.sync-generation-pending` from backup. `pair` remains a short `BEGIN IMMEDIATE` transaction independent of the generation lock. Mount the FCM secret read-only with permissions allowing uid 10001 to read it; verify readability without printing secret contents. Never recursively chown the host secret directory. Coordinator must run the disposable restore/failure-injection/root-owned-volume/entrypoint rehearsal before this runbook is accepted.

Restore into a scratch file to verify the backup:

```sh
docker run --rm -v /var/backups/bread-tracker:/b -v /tmp:/o litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b/litestream
sqlite3 /tmp/restored.db 'select count(*) from items;'
```
