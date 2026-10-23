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
docker compose -f packaging/compose.yaml --project-directory . stop litestream
docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker
docker run --rm --user 0 -v bread-tracker-data:/var/lib/bread-tracker \
  -v /var/backups/bread-tracker:/b:ro -v /tmp:/o \
  litestream/litestream:0.3.13 restore -o /o/restored.db file:///b/litestream
docker run --rm --user 0 -v bread-tracker-data:/var/lib/bread-tracker \
  -v /tmp:/o --entrypoint /usr/local/bin/entrypoint-exec bread-tracker:local \
  /bin/sh -c 'sqlite3 /var/lib/bread-tracker/bread-tracker.db "PRAGMA wal_checkpoint(TRUNCATE);" || true; cp /o/restored.db /var/lib/bread-tracker/bread-tracker.db; chown -R 10001:10001 /var/lib/bread-tracker'
docker run --rm --user 10001:10001 \
  -v bread-tracker-data:/var/lib/bread-tracker \
  --entrypoint /usr/local/bin/entrypoint-exec bread-tracker:local /usr/local/bin/bread-tracker-server mark-restored
docker compose -f packaging/compose.yaml --project-directory . up -d bread-tracker
docker compose -f packaging/compose.yaml --project-directory . up -d litestream
```

Use the actual Compose volume name and configured `BREADTRACKER_DATABASE_PATH` if they differ from these defaults. Ensure no root/sidecar-managed SQLite process still has the DB open before replacement. Do not copy a `.sync-generation` or `.sync-generation-pending` file from a backup. The generation is created automatically on first server startup; subsequent restarts preserve it. `mark-restored` is specifically for after replacing the database, not routine restarts. Server processes hold a shared OS lock for their lifetime, and `mark-restored` takes the exclusive lock, so it refuses to overlap an active server. The one-shot `pair` command uses a short `BEGIN IMMEDIATE` SQLite transaction and does not acquire the lifetime lock. Server startup reconciles pending generation journal state inside a write transaction before binding/listening. The marker applies migrations to the restored DB and refuses to run if the database file is absent. Run commands as the Compose owner; the image uses uid 10001. For an existing root-owned volume, the root-only upgrade helper above updates the DB file and assigns only the named data volume to uid 10001, leaving the separate FCM secret mount untouched; keep Litestream and server stopped during that ownership migration. Until marking, clients cannot reliably distinguish a same-generation restore whose sequence has caught up with their saved cursor.

Restore into a scratch file to verify the backup:

```sh
docker run --rm -v /var/backups/bread-tracker:/b -v /tmp:/o litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b/litestream
sqlite3 /tmp/restored.db 'select count(*) from items;'
```
