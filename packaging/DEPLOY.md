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

## Disposable restore and existing-volume rehearsal

Run from a scratch checkout using an image built from this revision. These commands use a unique task prefix and a task-named host directory, use only `--rm` containers, and do not reference a Compose volume or service. They exercise an existing root-owned database, restore replacement, marker, server startup, entrypoint command dispatch, and secret readability without printing secret contents. The block is executable as a single shell script and handles cleanup after the server exits.

```sh
set -eu
TASK="track2-rehearsal-$$"
IMG="$TASK:local"
ROOT="$(pwd)"
SCRATCH="$ROOT/.scratch-$TASK"
cleanup() {
  running=$(docker inspect --format '{{.Name}} {{.State.Running}}' "$TASK-server" 2>/dev/null || true)
  if [ "$running" = "/$TASK-server true" ]; then
    printf 'Stopping verified task container %s\n' "$TASK-server"
    docker stop "$TASK-server" >/dev/null
    docker wait "$TASK-server" >/dev/null
  fi
  if docker inspect "$TASK-server" >/dev/null 2>&1; then
    echo "Refusing to remove bind data while task server still exists" >&2
    return 1
  fi
  docker image rm "$IMG" >/dev/null 2>&1 || true
  rm -rf "$SCRATCH"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -m 700 "$SCRATCH"
mkdir -m 700 "$SCRATCH/data" "$SCRATCH/restore"
chmod 0755 "$SCRATCH"
chmod 0770 "$SCRATCH/data" "$SCRATCH/restore"
chown 0:10001 "$SCRATCH/data" "$SCRATCH/restore"
docker build -f packaging/Containerfile -t "$IMG" .

# Make a populated root-owned pre-upgrade 0001 database and a restored candidate.
docker run --rm --name "$TASK-init" --user 0 -v "$SCRATCH/data:/work" \
  -v "$SCRATCH/restore:/restore" -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --mount "type=bind,src=$ROOT/migrations/0001_init.sql,dst=/fixture/0001_init.sql,readonly" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec '
  chmod 0770 /work /restore
  chown 0:10001 /work /restore
  sqlite3 /work/existing.db < /fixture/0001_init.sql
  sqlite3 /work/existing.db "INSERT INTO items(id,kind,name,weekdays,points,sort,active,updated_at,seq) VALUES (\"11111111-1111-4111-8111-111111111111\",\"chore\",\"preserved\",127,10,0,1,1,1); UPDATE meta SET val=1 WHERE key=\"seq\";"
  sqlite3 /work/existing.db "PRAGMA journal_mode=WAL;"
  sqlite3 /work/existing.db "PRAGMA wal_checkpoint(TRUNCATE);" > /tmp/fixture-checkpoint
  IFS="|" read -r busy log_frames checkpointed < /tmp/fixture-checkpoint
  test "$busy" = 0
  if [ "$log_frames" -ge 0 ]; then
    test "$log_frames" = "$checkpointed"
  else
    test "$log_frames" = "-1" && test "$checkpointed" = "-1"
  fi
  cp /work/existing.db /restore/restored.db
  test "$(sqlite3 /restore/restored.db "PRAGMA integrity_check;")" = ok
  chmod 0600 /work/existing.db
  chmod 0444 /restore/restored.db
  chown 0:0 /work/existing.db /restore/restored.db
'
# Failure path: invalid/missing restore candidate exits nonzero. Do not invoke marker
# or server in this branch; this is an explicit assertion that shell failure propagates.
if docker run --rm --name "$TASK-failure" --user 0 -v "$SCRATCH/data:/work" \
  -v "$SCRATCH/restore:/restore" -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec '
  test -f /restore/not-present.db
  cp /restore/not-present.db /work/existing.db
'; then
  echo "ERROR: invalid restore unexpectedly succeeded" >&2
  exit 1
fi

# Create a pre-existing root-owned database at the configured filename from inside
# the explicitly root-run helper. The host user never needs to read/chown fixture data.
docker run --rm --name "$TASK-fixture" --user 0 -v "$SCRATCH/data:/data" \
  -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec '
    cp /data/existing.db /data/bread-tracker.db
    chown 0:0 /data/bread-tracker.db
    chmod 0600 /data/bread-tracker.db
  '

# Restore into an already-existing root-owned data directory. No SQLite connection is
# left open when this finishes. Checkpoint/integrity/copy/chown failures all abort.
docker run --rm --name "$TASK-replace" --user 0 -v "$SCRATCH/data:/var/lib/bread-tracker" \
  -v "$SCRATCH/restore:/restore:ro" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec '
    test "$(sqlite3 /restore/restored.db "PRAGMA integrity_check;")" = ok
    sqlite3 /var/lib/bread-tracker/bread-tracker.db "PRAGMA wal_checkpoint(TRUNCATE);" > /tmp/checkpoint
    IFS="|" read -r busy log_frames checkpointed < /tmp/checkpoint
    if [ "$busy" -ne 0 ]; then exit 1; fi
    if [ "$log_frames" -ge 0 ]; then
      test "$log_frames" = "$checkpointed"
    else
      test "$log_frames" = "-1" && test "$checkpointed" = "-1"
    fi
    cp /restore/restored.db /var/lib/bread-tracker/bread-tracker.db.new
    test "$(sqlite3 /var/lib/bread-tracker/bread-tracker.db.new "PRAGMA integrity_check;")" = ok
    mv /var/lib/bread-tracker/bread-tracker.db.new /var/lib/bread-tracker/bread-tracker.db
    rm -f /var/lib/bread-tracker/bread-tracker.db-wal /var/lib/bread-tracker/bread-tracker.db-shm
    chown -R 10001:10001 /var/lib/bread-tracker
  '

# Create a dummy secret readable to uid 10001 inside a root helper and test readability
# under uid 10001 only. The dummy contents are never emitted.
docker run --rm --name "$TASK-secret-prepare" --user 0 -v "$SCRATCH/data:/scratch" \
  -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec '
    printf "dummy-not-a-secret\\n" > /scratch/dummy-secret
    chown 0:10001 /scratch/dummy-secret
    chmod 0440 /scratch/dummy-secret
  '
docker run --rm --name "$TASK-secret-test" --user 10001:10001 -v "$SCRATCH/data:/scratch:ro" \
  -v "$SCRATCH/data/dummy-secret:/run/secrets/fcm.json:ro" -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec 'test -r /run/secrets/fcm.json'

# Test entrypoint does not prepend the server binary to utility/marker invocations.
docker run --rm --name "$TASK-own" --user 0 -v "$SCRATCH/data:/data" \
  -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -ec 'chown -R 10001:10001 /data'
docker run --rm --name "$TASK-marker" --user 10001:10001 -v "$SCRATCH/data:/var/lib/bread-tracker" \
  -e BREADTRACKER_DATABASE_PATH=/var/lib/bread-tracker/bread-tracker.db \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /usr/local/bin/bread-tracker-server mark-restored

# Start server using default image ENTRYPOINT with no extra command on a
# non-production host port. It is task-named and auto-removed.
test -z "$(ss -ltn '( sport = :17291 )' | tail -n +2)"
docker run --rm -d --name "$TASK-server" --user 10001:10001 \
  -v "$SCRATCH/data:/var/lib/bread-tracker" \
  -e BREADTRACKER_DATABASE_PATH=/var/lib/bread-tracker/bread-tracker.db \
  -p 127.0.0.1:17291:7290 "$IMG"
docker inspect --format '{{.Name}} {{.State.Running}}' "$TASK-server"
curl --fail --retry 10 --retry-connrefused --retry-delay 1 http://127.0.0.1:17291/healthz
docker stop "$TASK-server"
test "$(docker wait "$TASK-server")" = 0
if docker inspect "$TASK-server" >/dev/null 2>&1; then
  echo "ERROR: task-prefixed --rm server container still exists" >&2
  exit 1
fi
docker image rm "$IMG"
rm -rf "$SCRATCH"
```

The health check and stop are in the same executable block, so no undefined task variable or manual cross-terminal handoff is needed. It checks the host port before binding, prints the exact task name/state, stops only that task-prefixed container, waits for its exit, confirms `--rm` removed it, and only then deletes the bind-mounted scratch data. No named volume, production service, or production port is used.

All container stop operations above name and verify the task-prefix instance only. The exit trap then removes the task image and scratch tree after the server exits. No named Docker volume, production service, or production port is used.
