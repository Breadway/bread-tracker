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
set -eu
docker compose -f packaging/compose.yaml --project-directory . stop litestream
docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker
# Restore into a unique host scratch directory first. Do not point the output at the DB.
TASK="track2-restore-$(date +%s)-$$"
SCRATCH="/var/tmp/$TASK"
mkdir -m 700 "$SCRATCH"
docker run --rm --name "$TASK-litestream" --user 0 \
  -v /var/backups/bread-tracker:/b:ro -v "$SCRATCH:/o" \
  litestream/litestream:0.3.13 restore -o /o/restored.db file:///b/litestream
test "$(sqlite3 "$SCRATCH/restored.db" 'PRAGMA integrity_check;')" = ok
# All SQLite users must be stopped. Checkpoint errors or a busy WAL abort the command.
# Keep DB, WAL, SHM and candidate until replacement passes integrity_check. Change
# ownership of the complete data directory, including generation sidecars and lock files.
docker run --rm --name "$TASK-replace" --user 0 \
  -v bread-tracker-data:/data -v bread-tracker-data:/var/lib/bread-tracker -v "$SCRATCH:/restore" \
  --entrypoint /usr/local/bin/entrypoint-exec bread-tracker:local /bin/sh -ec '
    sqlite3 /data/bread-tracker.db "PRAGMA wal_checkpoint(TRUNCATE);" > /tmp/checkpoint
    IFS="|" read -r busy log_frames checkpointed < /tmp/checkpoint
    test "$busy" = 0
    if [ "$log_frames" = "-1" ]; then
      test "$checkpointed" = "-1"
    else
      test "$log_frames" = "$checkpointed"
    fi
    cp /restore/restored.db /data/bread-tracker.db.new
    test "$(sqlite3 /data/bread-tracker.db.new "PRAGMA integrity_check;")" = ok
    mv /data/bread-tracker.db.new /data/bread-tracker.db
    rm -f /data/bread-tracker.db-wal /data/bread-tracker.db-shm
    chown -R 10001:10001 /data
  '
# Change ownership of the full data directory before marker/server, not the separate
# FCM secret mount. This includes generation sidecars, lock files and SQLite files.
docker run --rm --name "$TASK-own" --user 0 -v bread-tracker-data:/data -v bread-tracker-data:/var/lib/bread-tracker \
  --entrypoint /usr/local/bin/entrypoint-exec bread-tracker:local \
  /bin/sh -ec 'chown -R 10001:10001 /data'
# Explicit ENTRYPOINT override is required: otherwise this command is appended to
# bread-tracker-server and mark-restored will not execute.
docker run --rm --name "$TASK-marker" --user 10001:10001 \
  -v bread-tracker-data:/var/lib/bread-tracker \
  --entrypoint /usr/local/bin/entrypoint-exec bread-tracker:local \
  /usr/local/bin/bread-tracker-server mark-restored
docker compose -f packaging/compose.yaml --project-directory . up -d litestream
docker compose -f packaging/compose.yaml --project-directory . up -d bread-tracker
rm -rf "$SCRATCH"
```

The normal operational restore above is intentionally separate from the disposable rehearsal below. Stop both services before copying. Do not copy a generation sidecar from backup. The marker creates `.sync-restore-failed` before changing generation metadata and removes it only after success. Server startup and a subsequent marker invocation fail closed if the sentinel remains. Preserve the database, WAL and SHM until the restored candidate has passed integrity validation and replacement succeeds. The restore command is executed by the Litestream image's own default entrypoint; the wrapper exists only in the tracker image. The replacement helper creates no DB connection besides the explicit checkpoint and copy-time validation, then changes ownership of the data directory, including sidecars and lock files, to uid 10001. Ensure the FCM secret file itself is readable by uid 10001 or its supplementary group before restart; do not recursively chown the secret path. `pair` uses a short SQLite transaction and does not take the generation lock.

## Disposable restore rehearsal

Run the standalone rehearsal from a scratch checkout built from this revision. It uses only task-prefixed `--rm` containers, bind-mounted scratch directories and host port 17291. It does not use Compose or named volumes. Do not run it against production. The script prepares fixture files and the separate dummy secret inside root-run helpers, so it requires no host `chown` or `sudo`.

```sh
scripts/rehearse_restore.sh
```

The script checkpoints and integrity-checks the candidate, validates it before replacement, verifies marker and server startup, and tests secret readability without printing its contents. It uses a valid SQLx migration ledger with the SHA-384 checksum of the exact embedded 0001 migration. The server runs in the foreground under a host background PID: after health succeeds, the script verifies the exact task container name, stops only that container, and waits on the host `docker run` process for the exit status. Cleanup refuses to remove bind data while that container remains, uses a root helper to restore host uid/gid ownership, then removes only its exact scratch path and image. Runtime Docker verification is deliberately separate from static CI checks.
