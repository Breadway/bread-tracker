# Deploying bread-tracker

```sh
rsync -a --delete --exclude target --exclude .git ./ your-server:~/bread-tracker/
ssh your-server 'cd ~/bread-tracker && mkdir -p /var/backups/bread-tracker 2>/dev/null; \
  docker compose -f packaging/compose.yaml --project-directory . up -d --build'
curl -s http://127.0.0.1:7290/healthz            # on the server: ok
docker compose -f packaging/compose.yaml --project-directory . exec bread-tracker bread-tracker-server pair   # one-time pairing code for the phone
```

Run compose as an ordinary user, not root.

## Edge

Put a reverse proxy with TLS in front of the container and forward your public name to port 7290. The compose file publishes `${BREADTRACKER_BIND:-127.0.0.1:7290}`, so by default only the host itself can reach it. If the proxy runs on another machine, set `BREADTRACKER_BIND` in `.env` to an address that machine can reach, preferably a private interface such as a VPN address, rather than `0.0.0.0`, which also serves plain HTTP on every interface.

## Push (FCM)

Put the Firebase service-account JSON on the server outside the repo (mode 600) and set in `.env`:

```
BREADTRACKER_FCM_KEY_FILE=/path/to/secrets/bread-tracker-fcm.json
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

Run the same rehearsal with an existing root-owned database/volume and confirm the marker and server can open it as uid 10001. Always use `set -e`; a failed checkpoint, integrity check, or copy must stop before marking or service restart. The marker creates a persistent `.sync-restore-failed` sentinel before modifying generation metadata and removes it only after successful publication. Server startup and subsequent marker attempts refuse while the sentinel remains. After failure, inspect/restore the database and sidecar state before operator-approved recovery; do not delete the sentinel as routine cleanup. Checkpoint and close every SQLite user before replacement; never ignore checkpoint errors or copy just the main DB while WAL is live. Verify the restored DB with `PRAGMA integrity_check` before replacement. Do not copy `.sync-generation` or `.sync-generation-pending` from backup. `pair` remains a short `BEGIN IMMEDIATE` transaction independent of the generation lock. Mount the FCM secret read-only with permissions allowing uid 10001 to read it; verify readability without printing secret contents. Never recursively chown the host secret directory. Coordinator must run the disposable restore/failure-injection/root-owned-volume/entrypoint rehearsal before this runbook is accepted.

Restore into a scratch file to verify the backup:

```sh
docker run --rm -v /var/backups/bread-tracker:/b -v /tmp:/o litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b/litestream
sqlite3 /tmp/restored.db 'select count(*) from items;'
```
