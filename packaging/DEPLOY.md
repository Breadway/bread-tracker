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
  --entrypoint /usr/local/bin/entrypoint-exec litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b/litestream
test "$(sqlite3 "$SCRATCH/restored.db" 'PRAGMA integrity_check;')" = ok
# All SQLite users must be stopped. Checkpoint errors or a busy WAL abort the command.
# Keep the original database and candidate until the replacement passes integrity_check.
docker run --rm --name "$TASK-replace" --user 0 \
  -v bread-tracker-data:/data -v "$SCRATCH:/restore" \
  --entrypoint /usr/local/bin/entrypoint-exec bread-tracker:local /bin/sh -ec '
    sqlite3 /data/bread-tracker.db "PRAGMA wal_checkpoint(TRUNCATE);" > /tmp/checkpoint
    grep -qx "0|0|0" /tmp/checkpoint
    cp /restore/restored.db /data/bread-tracker.db.new
    test "$(sqlite3 /data/bread-tracker.db.new "PRAGMA integrity_check;")" = ok
    chown 10001:10001 /data/bread-tracker.db.new
    mv /data/bread-tracker.db.new /data/bread-tracker.db
    chown 10001:10001 /data/bread-tracker.db
  '
# Explicit ENTRYPOINT override is required: otherwise this command is appended to
# bread-tracker-server and mark-restored will not execute.
docker run --rm --name "$TASK-marker" --user 10001:10001 \
  -v bread-tracker-data:/var/lib/bread-tracker \
  --entrypoint /usr/local/bin/entrypoint-exec bread-tracker:local \
  /usr/local/bin/bread-tracker-server mark-restored
docker compose -f packaging/compose.yaml --project-directory . up -d bread-tracker
docker compose -f packaging/compose.yaml --project-directory . up -d litestream
rm -rf "$SCRATCH"
```

The normal operational restore above is intentionally separate from the disposable rehearsal below. Stop both services before copying. Do not copy a generation sidecar from backup. The marker creates `.sync-restore-failed` before changing generation metadata and removes it only after success. Server startup and a subsequent marker invocation fail closed if the sentinel remains. Preserve the database, WAL and SHM until the restored candidate has passed integrity validation and replacement succeeds. `pair` uses a short SQLite transaction and does not take the generation lock.

## Disposable restore and existing-volume rehearsal

Run from a scratch checkout using an image built from this revision. These commands use a unique task prefix and a task-named host directory, use only `--rm` containers, and do not reference a Compose volume or service. They exercise an existing root-owned database, restore replacement, marker, server startup, entrypoint command dispatch, and secret readability without printing secret contents. Keep the disposable server in the foreground. Stop only that task-named instance with Ctrl-C when the health probe passes.

```sh
set -eu
TASK="track2-rehearsal-$$"
IMG="$TASK:local"
ROOT="$(pwd)"
SCRATCH="$ROOT/.scratch-$TASK"
trap 'rm -rf "$SCRATCH"' EXIT
mkdir -m 700 "$SCRATCH"
mkdir -m 700 "$SCRATCH/data" "$SCRATCH/restore"
docker build -f packaging/Containerfile -t "$IMG" .

# Make a populated root-owned pre-upgrade 0001 database and a restored candidate.
docker run --rm --name "$TASK-init" --user 0 -v "$SCRATCH/data:/work" -v "$ROOT:/src:ro" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec '
  sqlite3 /work/existing.db < /src/migrations/0001_init.sql
  sqlite3 /work/existing.db "INSERT INTO items(id,kind,name,weekdays,points,sort,active,updated_at,seq) VALUES (\"11111111-1111-4111-8111-111111111111\",\"chore\",\"preserved\",127,10,0,1,1,1); UPDATE meta SET val=1 WHERE key=\"seq\";"
  cp /work/existing.db /work/restored.db
  test "$(sqlite3 /work/restored.db "PRAGMA integrity_check;")" = ok
  chmod 600 /work/existing.db /work/restored.db
  chown 0:0 /work/existing.db /work/restored.db
'

# Failure path: invalid/missing restore candidate exits nonzero. Do not invoke marker
# or server in this branch; this is an explicit assertion that shell failure propagates.
if docker run --rm --name "$TASK-failure" --user 0 -v "$SCRATCH/data:/work" \
  -v "$SCRATCH/restore:/restore:ro" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec '
  test -f /restore/not-present.db
  cp /restore/not-present.db /work/existing.db
'; then
  echo "ERROR: invalid restore unexpectedly succeeded" >&2
  exit 1
fi

# Put the pre-existing root-owned DB fixture at the configured filename.
cp "$SCRATCH/data/existing.db" "$SCRATCH/data/bread-tracker.db"
chown 0:0 "$SCRATCH/data/bread-tracker.db"
cp "$SCRATCH/data/restored.db" "$SCRATCH/restore/restored.db"
chmod 0444 "$SCRATCH/restore/restored.db"
chown 0:0 "$SCRATCH/restore/restored.db"

# Restore into an already-existing root-owned data directory. No SQLite connection is
# left open when this finishes. Checkpoint/integrity/copy/chown failures all abort.
docker run --rm --name "$TASK-replace" --user 0 -v "$SCRATCH/data:/var/lib/bread-tracker" -v "$SCRATCH/restore:/restore:ro" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec '
    test "$(sqlite3 /restore/restored.db "PRAGMA integrity_check;")" = ok
    sqlite3 /var/lib/bread-tracker/bread-tracker.db "PRAGMA wal_checkpoint(TRUNCATE);" > /tmp/checkpoint
    grep -q "0|0|0" /tmp/checkpoint
    cp /restore/restored.db /var/lib/bread-tracker/bread-tracker.db.new
    test "$(sqlite3 /var/lib/bread-tracker/bread-tracker.db.new "PRAGMA integrity_check;")" = ok
    chown 10001:10001 /var/lib/bread-tracker/bread-tracker.db.new
    mv /var/lib/bread-tracker/bread-tracker.db.new /var/lib/bread-tracker/bread-tracker.db
    rm -f /var/lib/bread-tracker/bread-tracker.db-wal /var/lib/bread-tracker/bread-tracker.db-shm
    chown 10001:10001 /var/lib/bread-tracker/bread-tracker.db
  '

# Create a dummy secret readable to uid 10001 and verify only readability, never contents.
printf 'dummy-only-not-a-real-secret\n' > "$SCRATCH/dummy-secret"
chmod 0440 "$SCRATCH/dummy-secret"
chown 0:10001 "$SCRATCH/dummy-secret"
docker run --rm --name "$TASK-secret" --user 10001:10001 -v "$SCRATCH/data:/var/lib/bread-tracker" -v "$SCRATCH/dummy-secret:/run/secrets/fcm.json:ro" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec 'test -r /run/secrets/fcm.json'

# Test entrypoint does not prepend the server binary to utility/marker invocations.
docker run --rm --name "$TASK-marker" --user 10001:10001 -v "$SCRATCH/data:/var/lib/bread-tracker" \
  -e BREADTRACKER_DATABASE_PATH=/var/lib/bread-tracker/bread-tracker.db \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /usr/local/bin/bread-tracker-server mark-restored

# Start server using default image ENTRYPOINT with no extra command. This container
# stays in the foreground; in another terminal run the health probe, then Ctrl-C it.
docker run --rm --name "$TASK-server" --user 10001:10001 \
  -v "$SCRATCH/data:/var/lib/bread-tracker" \
  -e BREADTRACKER_DATABASE_PATH=/var/lib/bread-tracker/bread-tracker.db \
  -p 127.0.0.1:7290:7290 "$IMG"
```

When the foreground `docker run` is serving, open another terminal in the same checkout and run the health probe. Once it succeeds, return to the foreground terminal and press Ctrl-C, then verify the named disposable instance exited:

```sh
curl --fail http://127.0.0.1:7290/healthz
docker inspect --format '{{.State.Running}}' "$TASK-server"
```

After it exits, in the first terminal:

```sh
docker image rm "$IMG"
rm -rf "$SCRATCH"
```

These commands address only the task-prefixed image and scratch directory created above. No named Docker volume is created, and no production container or volume is accessed.
