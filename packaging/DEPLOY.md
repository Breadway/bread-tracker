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

## Upgrading an existing single-user deployment to the multi-user schema

This release adds migrations `0002_users.sql`, `0005_users_forward_repair.sql` and
`0006_users_name_key_guard.sql` and `0007_user_settings.sql`, applied automatically at server start, each in its own
transaction. 0002 adds a `users` table, gives all existing data to the first user (the admin)
and rebuilds `items`, `completions`, `devices`, `reminders_sent` and `pairing_codes` with a
`user_id`. 0005 restores the scheduler indexes the rebuild drops and adds a name key. 0006
tightens the name-key guard. Phones keep working without an app update: the sync API did not
change.

**This source branch is not a deployment approval.** Routes, reminder settings and scheduler
are per-user here, but the upstream candidates and the rest of the multi-user stack still need
integration and independent acceptance. Migration 0007 adopts existing admin preferences.
Old experimental settings003 ledgers are deliberately unsupported: preserve the old file and
create a new empty path ONLY for a positively identified task-owned disposable experiment.
Never modify its ledger, recreate production, or remove reviewer evidence. See DESIGN.md.

### What can and cannot be undone

* Migrations are not reversible, and an older image cannot open an upgraded database.
* **A failed migration does not restore the original database.** Each migration commits on its
  own: if 0005 or 0006 fails, 0002 stays committed. The server then refuses to start and the
  database is a valid intermediate state; fixing the cause and starting again completes the
  remaining migrations. Going back to the previous release always means restoring the full
  copy from step 2, never "the database is unchanged".
* **Writes made after the copy are lost by a rollback.** The copy is the recovery point. This
  runbook therefore keeps every writer stopped from the copy until the new release is up, so
  nothing is written in between. Once the new release has served phones, rolling back loses
  whatever they synced since. Phones keep their own data, but rows that already synced are not
  sent again, so do not roll back after live use unless you accept that loss; fix forward
  instead. Old-client reconciliation after a restore has not been tested.
* The Litestream replica is a second copy, not the recovery point, and it is only trusted after
  a mandatory gate (step 3) shows it holds exactly the same data as the frozen copy. Its file
  names and modification times prove nothing about what is inside, so the gate compares the
  data itself. Only the frozen copy from step 2 is authoritative.

Everything below uses only what this repository ships: the pinned Litestream image (it has
`sh`, `cp`, `tar`) and `python3` on the host with its standard `sqlite3` module, driven by
`scripts/db-evidence.py` and `scripts/backup-gate.sh` (standard library only; the server image has no SQLite
command line). Run it as the ordinary deploy user, from the checkout, with the
default compose project name. Plan a short maintenance window: the app is unavailable between
step 2 and step 4 and phones retry on their own.

1. **Record what is running.** Pin the current image under a name that building cannot
   overwrite (`up --build` replaces `bread-tracker:local`), and find the data volume:

   ```sh
   docker tag "$(docker inspect --format '{{.Image}}' bread-tracker)" bread-tracker:pre-multiuser
   docker image inspect --format '{{.Id}}' bread-tracker:pre-multiuser    # note this id
   VOL=$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/var/lib/bread-tracker"}}{{.Name}}{{end}}{{end}}' bread-tracker)
   echo "data volume: $VOL"                                               # must not be empty
   ```

2. **Freeze writers and take the recovery copy.** Stop the server first, give Litestream a
   moment to ship the last changes, then stop it too; a copy of a live database file is not a
   backup. The sleep is only a courtesy to the replica: step 3 is what proves it. The directory
   name is unique, so an earlier attempt can never be mistaken for this one:

   ```sh
   COPY=$HOME/bread-tracker-pre-multiuser-$(date +%Y%m%d-%H%M%S)
   mkdir "$COPY"
   docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker
   sleep 10
   docker compose -f packaging/compose.yaml --project-directory . stop litestream
   docker run --rm -v "$VOL":/data:ro -v "$COPY":/out \
     --entrypoint sh litestream/litestream:0.3.13 -c 'cp -a /data/. /out/'
   ls -l "$COPY"
   ```

   Do not start the containers again until step 4. Verify the copy itself. It must open,
   pass `integrity_check` and `foreign_key_check`, have a valid migration ledger (every
   migration recorded once and successful, with the checksum of the shipped file), and record
   only migrations this release supports (`1`, `2`, `3`, `5`, `6`, `7`):

   ```sh
   python3 scripts/db-evidence.py check "$COPY/bread-tracker.db" --ledger-subset 1,2,3,5,6,7
   ```

   It prints the evidence (row counts, `meta.seq`, the migration ledger, a digest of every
   row) and ends with `CHECK PASSED`. The checker works on a private copy of the database file
   and of its `-wal` and `-shm` files, so the files it is pointed at, sidecars included, stay
   byte-identical. Anything else, including a recorded migration this release
   does not ship such as `0004`, is a stop: leave the copy as it is, go to "If the gate or
   the upgrade fails" below, and do not deploy.

3. **Mandatory backup gate: the Litestream replica must restore and match the frozen copy.**
   The original requirement for this upgrade is a Litestream-checked backup before deploying.
   `scripts/backup-gate.sh` restores the replica into a scratch file and compares it with the
   frozen copy. Its exit status is the gate: `0` and `BACKUP GATE PASSED` only if the restore
   and the comparison both succeed, otherwise `BACKUP GATE FAILED: do not deploy` and a
   non-zero status:

   ```sh
   BACKUP_DIR=$(realpath "${BREADTRACKER_BACKUP_DIR:-./backups}")
   scripts/backup-gate.sh "$COPY/bread-tracker.db" "$BACKUP_DIR"
   ```

   Proceed to step 4 only if this exits `0` and prints `BACKUP GATE PASSED`. If you run these
   blocks from one script, run it with `set -e` or chain the deploy behind the gate
   (`scripts/backup-gate.sh ... && docker compose ... up -d --build`), so a failed gate can
   never reach the deploy command. The gate fails closed: a missing or empty replica
   directory, a restore error, an unreadable or empty restored file, a failed integrity or
   foreign key check, an invalid or different migration ledger (a missing ledger, a failed or
   duplicated migration, a checksum that differs from the frozen copy or from the shipped
   migration file), and any difference in `meta.seq`, row counts or row contents (a stale
   replica) all stop it. Nothing about file times is used. If it fails, do not deploy and do
   not delete or edit `$COPY`: it stays the authoritative recovery point while you repair
   replication (see below).

4. **Name the first user and deploy.** Only after `BACKUP GATE PASSED`. Set
   `BREADTRACKER_ADMIN_NAME` in `.env` (for example `Riley`), then build and start the new
   release:

   ```sh
   docker compose -f packaging/compose.yaml --project-directory . up -d --build
   curl -s http://127.0.0.1:7290/healthz        # ok (use your BREADTRACKER_BIND)
   docker logs bread-tracker | tail             # no migration error
   ```

   The name is applied once: it takes effect only while the first user has never been named,
   and later restarts with the same or another value change nothing. If it is unset the user
   is called `admin` and can still be named by a later start. A name another user already
   holds stops startup with a message.

   This release ships migrations `1`, `2`, `3`, `5`, `6`, `7`, `8`, `9`, `10` and `11`. A database that has already
   recorded `4` (the durable pairing limiter, PR31) is refused at start with a missing
   migration error and is not served. Supporting `4` needs a separately accepted PR31 that is
   integrated with this change so its migration and runtime ship together; this release does
   not include them, and the test fixture for `4` in the repository only exercises the ledger.

5. **Verify the result.** The new release and Litestream are now running. Give Litestream a
   few seconds, then restore the replica into a new scratch directory, as in step 3, and check
   it against the release's exact ledger:

   ```sh
   WORK=$(mktemp -d)
   docker run --rm --user "$(id -u):$(id -g)" -v "$BACKUP_DIR":/b:ro -v "$WORK":/o \
     litestream/litestream:0.3.13 restore -o /o/replica.db file:///b/litestream
   python3 scripts/db-evidence.py check "$WORK/replica.db" --ledger-exact 1,2,3,5,6,7,8,9,10,11
   ```

   Expect `CHECK PASSED` and the same counts for `items`, `completions`, `devices`,
   `reminders_sent` and `pairing_codes` as in step 2 (plus whatever phones synced since). The
   admin name is applied at start; confirm it in the server log or with the evidence output of
   a stopped copy of the volume.

### If the gate or the upgrade fails

*Before step 4 (the gate or the copy check failed).* Nothing has been changed. Bring the
current release back and fix the cause; repeat from step 2 with a new `$COPY` once replication
is healthy (the old one can be kept or removed by you afterwards):

```sh
docker compose -f packaging/compose.yaml --project-directory . start bread-tracker litestream
docker logs bread-tracker-litestream | tail
```

*After step 4 (the upgrade failed or must be undone).* Stay in the maintenance window. Stop
both containers, restore the frozen copy over the data files, put the pinned image back under
the name compose uses, and start without building:

```sh
docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker litestream
docker run --rm -v "$VOL":/data -v "$COPY":/in:ro \
  --entrypoint sh litestream/litestream:0.3.13 -c 'rm -f /data/bread-tracker.db* && cp -a /in/. /data/'
docker tag bread-tracker:pre-multiuser bread-tracker:local
docker compose -f packaging/compose.yaml --project-directory . up -d --no-build
curl -s http://127.0.0.1:7290/healthz
```

Then show the restored database's evidence from a stopped copy of the volume, as in step 2,
and compare with the step 2 output: the same counts, `meta.seq`, migrations and data digest.
Keep
`$COPY` and `bread-tracker:pre-multiuser` until the new release has run well. Litestream
notices that the database file was replaced and begins a new generation; confirm with
`docker logs bread-tracker-litestream`. The sync generation used by phones is the
separate `.sync-generation` sidecar rotated by `mark-restored`, not Litestream's own
generation. A full Compose restart of the old
image with Litestream resuming a generation, and phone recovery after a restore, are operator
deployment checks that have not been run for this change; they need no other pull request to
be merged first.

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

If a marker fails after it has written `.sync-restore-failed`, the server and a new marker both refuse to run. Do not delete the database. Find the failing command's output, stop both services, and verify the restored database with

```sh
python3 scripts/db-evidence.py check "$SCRATCH/restored.db" --ledger-subset 1,2,3,5,6,7
```

Only after that check passes, remove the sentinel and the pending journal (if present), then rerun the marker against the same database path:

```sh
rm -f /var/lib/bread-tracker/bread-tracker.db.sync-restore-failed \
      /var/lib/bread-tracker/bread-tracker.db.sync-generation-pending
docker run --rm --name "$TASK-marker" --user 10001:10001 \
  -v bread-tracker-data:/var/lib/bread-tracker \
  --entrypoint /usr/local/bin/entrypoint-exec bread-tracker:local \
  /usr/local/bin/bread-tracker-server mark-restored
```

A pending journal alone is recoverable by normal startup, which rolls it forward; the sentinel is what stops that, so remove the sentinel last and only after the evidence check.

The normal operational restore above is intentionally separate from the disposable rehearsal below. Stop both services before copying. Do not copy a generation sidecar from backup. The marker creates `.sync-restore-failed` before changing generation metadata and removes it only after success. Server startup and a subsequent marker invocation fail closed if the sentinel remains. Preserve the database, WAL and SHM until the restored candidate has passed integrity validation and replacement succeeds. The restore command is executed by the Litestream image's own default entrypoint; the wrapper exists only in the tracker image. The replacement helper creates no DB connection besides the explicit checkpoint and copy-time validation, then changes ownership of the data directory, including sidecars and lock files, to uid 10001. Ensure the FCM secret file itself is readable by uid 10001 or its supplementary group before restart; do not recursively chown the secret path. `pair` uses a short SQLite transaction and does not take the generation lock.

## Disposable restore rehearsal

Run the standalone rehearsal from a scratch checkout built from this revision. It uses only task-prefixed `--rm` containers, bind-mounted scratch directories and host port 17291. It does not use Compose or named volumes. Do not run it against production. The script prepares fixture files and the separate dummy secret inside root-run helpers, so it requires no host `chown` or `sudo`.

```sh
scripts/rehearse_restore.sh
```

The script checkpoints and integrity-checks the candidate, validates it before replacement, verifies marker and server startup, and tests secret readability without printing its contents. It uses a valid SQLx migration ledger with the SHA-384 checksum of the exact embedded 0001 migration. The server runs in the foreground under a host background PID: after health succeeds, the script verifies the exact task container name, stops only that container, and waits on the host `docker run` process for the exit status. Cleanup refuses to remove bind data while that container remains, uses a root helper to restore host uid/gid ownership, then removes only its exact scratch path and image. Runtime Docker verification is deliberately separate from static CI checks.
## Invite signup admission

This candidate adds migration 0008 for invites and 0009 for shared pairing/signup failure budgets. Set `BREADTRACKER_TRUSTED_PROXIES` only to the verified transport peer IPs or CIDRs of your sanitizing proxy. By default forwarded headers are ignored. Check the actual application peer and effective header chain before deployment; do not trust a client-supplied forwarded address. Pairing and signup share five failures per source per ten minutes and a 1,024-source admission cap. Successful attempts do not spend failures. Old experimental invite004 ledgers are unsupported; preserve their evidence and use a new path only for positively identified disposable experiments. Never rewrite an applied production migration ledger.

## Users, invites and devices

```sh
docker exec bread-tracker bread-tracker-server invite create [--ttl 7d] # prints a single-use code, once
docker exec bread-tracker bread-tracker-server invite list              # state only, never the code
docker exec bread-tracker bread-tracker-server invite revoke <id>
docker exec bread-tracker bread-tracker-server user list
docker exec bread-tracker bread-tracker-server devices [--user NAME]
docker exec bread-tracker bread-tracker-server pair [--user NAME]    # a code for that user's new device
docker exec bread-tracker bread-tracker-server revoke <device id>
docker exec bread-tracker bread-tracker-server user export NAME     # everything stored for a user, as JSON
docker exec bread-tracker bread-tracker-server user disable NAME
docker exec bread-tracker bread-tracker-server user enable NAME
docker exec bread-tracker bread-tracker-server user remove NAME --yes # deletes the user and all their data
```

Without `--user`, `pair` makes a code for the admin. Users add their own second device from the app (Settings), which calls `POST /pair-code`; you do not need to be involved. `user remove` cannot be undone, so take a backup first if in doubt; it refuses to remove an admin. `user disable` stops an account without deleting anything.

Send an invite code and the server address to the new person through a private channel, then give them `docs/ONBOARDING.md`: it covers installing the APK, joining with the invite, adding a second device, and what you as the server owner can see.

## Limits

The defaults suit a small shared server (20 users, 2000 items and 50000 completions each, 10 devices each, 120 requests a minute per device). Change them in `.env` with `BREADTRACKER_MAX_USERS`, `BREADTRACKER_MAX_ITEMS_PER_USER`, `BREADTRACKER_MAX_COMPLETIONS_PER_USER`, `BREADTRACKER_MAX_DEVICES_PER_USER` and `BREADTRACKER_REQUESTS_PER_MINUTE`. The existing deployment is well below them. To stop someone without deleting their data: `user disable NAME` (and `user enable NAME`).
