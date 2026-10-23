#!/bin/sh
set -eu
TASK="track2-rehearsal-$(date +%s)-$$"
IMG="$TASK:local"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SCRATCH="$ROOT/.scratch-$TASK"
EVIDENCE_DIR=${TRACK2_REHEARSAL_EVIDENCE_DIR:-/tmp/opencode/track2-rehearsal-evidence}
EVIDENCE_LOG="$EVIDENCE_DIR/$TASK-server.log"
SERVER_PID=
SERVER_STARTED=0
cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [ "$SERVER_STARTED" -eq 1 ]; then
    name=$(docker inspect --format '{{.Name}}' "$TASK-server" 2>/dev/null || true)
    if [ "$name" = "/$TASK-server" ]; then
      printf 'Stopping verified task container %s\n' "$TASK-server"
      docker stop "$TASK-server" >/dev/null || status=1
    fi
    if [ -n "$SERVER_PID" ]; then
      set +e; wait "$SERVER_PID"; server_status=$?; set -e
      printf 'Foreground docker run exited with status %s\n' "$server_status"
    fi
    if docker inspect "$TASK-server" >/dev/null 2>&1; then
      echo 'Refusing cleanup while task server container still exists' >&2
      exit 1
    fi
  fi
  if [ -d "$SCRATCH" ] && docker image inspect "$IMG" >/dev/null 2>&1; then
    docker run --rm --name "$TASK-cleanup" --user 0 \
      -v "$SCRATCH/data:/data" -v "$SCRATCH/restore:/restore" -v "$SCRATCH/secret:/secret" \
      -v "$SCRATCH/data:/var/lib/bread-tracker" --entrypoint /usr/local/bin/entrypoint-exec "$IMG" \
      /bin/sh -exc "chown -R $(id -u):$(id -g) /data /restore /secret; chmod 0700 /data /restore /secret"
  fi
  docker image rm "$IMG" >/dev/null 2>&1 || true
  rm -rf -- "$SCRATCH"
  printf 'server diagnostics: %s\n' "$EVIDENCE_LOG" >&2
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -m 700 "$SCRATCH"
mkdir -m 700 "$SCRATCH/data" "$SCRATCH/restore" "$SCRATCH/secret"
chmod 0755 "$SCRATCH"
docker build -f "$ROOT/packaging/Containerfile" -t "$IMG" "$ROOT"
echo 'stage: image built'
CHECKSUM=$(python3 -c 'import hashlib,sys; print(hashlib.sha384(open(sys.argv[1],"rb").read()).hexdigest())' "$ROOT/migrations/0001_init.sql")
cat >"$SCRATCH/fixture.sh" <<'FIXTURE'
set -eu
chmod 0770 /data
sqlite3 /data/bread-tracker.db < /fixture/0001_init.sql
sqlite3 /data/bread-tracker.db 'CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL);'
sqlite3 /data/bread-tracker.db "INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES (1,'init',1,X'$CHECKSUM',0);"
sqlite3 /data/bread-tracker.db "INSERT INTO items(id,kind,name,weekdays,points,sort,active,updated_at,seq) VALUES ('11111111-1111-4111-8111-111111111111','chore','preserved',127,10,0,1,1,1); UPDATE meta SET val=1 WHERE key='seq';"
chmod 0600 /data/bread-tracker.db
chown -R 0:0 /data
FIXTURE
docker run --rm --name "$TASK-fixture" --user 0 -e CHECKSUM="$CHECKSUM" -v "$SCRATCH/data:/data" \
  -v "$SCRATCH/fixture.sh:/fixture-helper.sh:ro" -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --mount "type=bind,src=$ROOT/migrations/0001_init.sql,dst=/fixture/0001_init.sql,readonly" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -exc 'exec /bin/sh /fixture-helper.sh'
echo 'stage: populated 0001 fixture created'
cat >"$SCRATCH/candidate.sh" <<'CANDIDATE'
set -eu
sqlite3 /data/bread-tracker.db 'PRAGMA journal_mode=WAL;' >/dev/null
sqlite3 /data/bread-tracker.db 'PRAGMA wal_checkpoint(TRUNCATE);' >/tmp/cp
IFS='|' read -r busy log_frames checkpointed </tmp/cp
test "$busy" = 0
if [ "$log_frames" -ge 0 ]; then test "$log_frames" = "$checkpointed"; else test "$log_frames" = -1 && test "$checkpointed" = -1; fi
# VACUUM INTO creates an independent single-file SQLite snapshot from the checkpointed database.
sqlite3 /data/bread-tracker.db "VACUUM INTO '/restore/restored.db';"
test "$(sqlite3 -readonly /restore/restored.db 'PRAGMA integrity_check;')" = ok
test "$(sqlite3 -readonly /restore/restored.db 'SELECT name FROM items;')" = preserved
test -z "$(find /restore -maxdepth 1 -name 'restored.db-*' -print)"
chown -R 0:0 /restore
CANDIDATE
docker run --rm --name "$TASK-candidate" --user 0 -v "$SCRATCH/data:/data" -v "$SCRATCH/restore:/restore" \
  -v "$SCRATCH/candidate.sh:/candidate-helper.sh:ro" -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -exc 'exec /bin/sh /candidate-helper.sh'
echo 'stage: standalone restore candidate integrity and row checks passed'
cat >"$SCRATCH/invalid.sh" <<'INVALID'
set -eu
before=$(sqlite3 /data/bread-tracker.db "SELECT name FROM items WHERE id='11111111-1111-4111-8111-111111111111';")
test "$before" = preserved
if [ "$(sqlite3 /restore/invalid.db 'PRAGMA integrity_check;' 2>/dev/null || true)" = ok ]; then exit 1; fi
after=$(sqlite3 /data/bread-tracker.db "SELECT name FROM items WHERE id='11111111-1111-4111-8111-111111111111';")
test "$after" = "$before"
INVALID
docker run --rm --name "$TASK-invalid-fixture" --user 0 -v "$SCRATCH/restore:/restore" -v "$SCRATCH/restore:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -exc 'printf "invalid db\\n" >/restore/invalid.db; chown 0:0 /restore/invalid.db'
docker run --rm --name "$TASK-invalid-check" --user 0 -v "$SCRATCH/data:/data" -v "$SCRATCH/restore:/restore:ro" \
  -v "$SCRATCH/invalid.sh:/invalid-helper.sh:ro" -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -exc 'exec /bin/sh /invalid-helper.sh'
echo 'stage: invalid candidate rejected without changing existing DB'
cat >"$SCRATCH/restore.sh" <<'RESTORE'
set -eu
test "$(sqlite3 -readonly /restore/restored.db 'PRAGMA integrity_check;')" = ok
sqlite3 /data/bread-tracker.db 'PRAGMA wal_checkpoint(TRUNCATE);' >/tmp/cp
IFS='|' read -r busy log_frames checkpointed </tmp/cp
test "$busy" = 0
if [ "$log_frames" -ge 0 ]; then test "$log_frames" = "$checkpointed"; else test "$log_frames" = -1 && test "$checkpointed" = -1; fi
test "$(sqlite3 -readonly /restore/restored.db 'SELECT name FROM items;')" = preserved
cp /restore/restored.db /data/bread-tracker.db.new
test "$(sqlite3 /data/bread-tracker.db.new 'PRAGMA integrity_check;')" = ok
mv /data/bread-tracker.db.new /data/bread-tracker.db
rm -f /data/bread-tracker.db-wal /data/bread-tracker.db-shm
printf 'dummy-not-a-secret\n' >/secret/fcm.json
chown 0:10001 /secret/fcm.json; chmod 0440 /secret/fcm.json
chown -R 10001:10001 /data
RESTORE
docker run --rm --name "$TASK-restore" --user 0 -v "$SCRATCH/data:/data" -v "$SCRATCH/restore:/restore:ro" \
  -v "$SCRATCH/restore.sh:/restore-helper.sh:ro" -v "$SCRATCH/secret:/secret" -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -exc 'exec /bin/sh /restore-helper.sh'
echo 'stage: replacement integrity, preserved row, separate secret prepared'
docker run --rm --name "$TASK-secret" --user 10001:10001 -v "$SCRATCH/data:/var/lib/bread-tracker" \
  -v "$SCRATCH/secret/fcm.json:/run/secrets/fcm.json:ro" --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -exc 'test -r /run/secrets/fcm.json'
docker run --rm --name "$TASK-marker" --user 10001:10001 -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /usr/local/bin/bread-tracker-server mark-restored
echo 'stage: secret readability and marker passed'
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
if ! ss -H -ltn '( sport = :17291 )' >"$EVIDENCE_DIR/$TASK-portcheck.txt" 2>&1; then
  cat "$EVIDENCE_DIR/$TASK-portcheck.txt" >&2
  echo 'Port 17291 preflight failed; not starting rehearsal server' >&2
  exit 1
fi
if [ -s "$EVIDENCE_DIR/$TASK-portcheck.txt" ]; then
  cat "$EVIDENCE_DIR/$TASK-portcheck.txt" >&2
  echo 'Port 17291 is already occupied; not starting rehearsal server' >&2
  exit 1
fi
echo 'stage: port 17291 preflight passed'
docker run --rm --name "$TASK-server" --user 10001:10001 -v "$SCRATCH/data:/var/lib/bread-tracker" \
  -e BREADTRACKER_DATABASE_PATH=/var/lib/bread-tracker/bread-tracker.db -p 127.0.0.1:17291:7290 "$IMG" >"$EVIDENCE_LOG" 2>&1 &
SERVER_PID=$!
SERVER_STARTED=1
attempt=0
until curl --fail --silent --show-error --max-time 2 http://127.0.0.1:17291/healthz >"$EVIDENCE_DIR/$TASK-health.txt" 2>&1; do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    cat "$EVIDENCE_LOG" >&2
    set +e; wait "$SERVER_PID"; startup_status=$?; set -e
    echo "Rehearsal server exited before health probe; foreground status $startup_status" >&2
    exit 1
  fi
  attempt=$((attempt + 1)); [ "$attempt" -lt 30 ] || { cat "$EVIDENCE_DIR/$TASK-health.txt" >&2; cat "$EVIDENCE_LOG" >&2; echo 'Health probe timed out' >&2; exit 1; }
  sleep 1
done
echo 'stage: health check passed'
name=$(docker inspect --format '{{.Name}}' "$TASK-server")
case "$name" in "/$TASK-server") printf 'Stopping verified task container %s\n' "$TASK-server" ;; *) echo 'Unexpected server container name' >&2; exit 1 ;; esac
docker stop "$TASK-server" >/dev/null
set +e; wait "$SERVER_PID"; server_status=$?; set -e
SERVER_PID=; SERVER_STARTED=0
if docker inspect "$TASK-server" >/dev/null 2>&1; then echo 'server container was not auto-removed' >&2; exit 1; fi
printf 'Server foreground exit status: %s\n' "$server_status"
test "$server_status" -eq 0
docker run --rm --name "$TASK-assert" --user 0 -v "$SCRATCH/data:/data" -v "$SCRATCH/data:/var/lib/bread-tracker" \
  --entrypoint /usr/local/bin/entrypoint-exec "$IMG" /bin/sh -exc "test \"\$(sqlite3 /data/bread-tracker.db 'SELECT name FROM items;')\" = preserved"
echo 'stage: server exit status zero and restored row preserved'
