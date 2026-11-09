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

This stack adds migrations `0002_users.sql`, `0004_pair_rate_limits.sql`,
`0005_users_forward_repair.sql`, `0006_users_name_key_guard.sql`,
`0007_user_settings.sql`, `0008_invites.sql`, `0009_public_auth_limits.sql`,
`0010_user_disabled.sql`, `0011_authenticated_rate_limits.sql`,
`0012_install_provenance.sql`, `0013_item_created_on.sql` and
`0014_stats_ledger.sql`. They are applied automatically at server start, each in its own
transaction. The embedded set is `1,2,3,4,5,6,7,8,9,10,11,12,13,14,14`. 0004's
`pair_rate_limits` table stays reserved and has no writer: the live pairing and signup
admission is 0009's `public_auth_limits`. A database that already recorded 0004 is adopted
by the same start, keeping its checksum and table. 0002 adds a `users` table, gives all existing data to the first user (the admin) and
rebuilds `items`, `completions`, `devices`, `reminders_sent` and `pairing_codes` with a
`user_id`. 0005 restores the scheduler indexes the rebuild drops and adds a name key. 0006
tightens the name-key guard. 0012 records whether the database was an empty file before this
run and gives only a genuinely new installation the neutral UTC default; an existing household
keeps its timezone even when it has no devices left. 0013 adds the nullable `items.created_on`
creation date; existing rows stay NULL, meaning no lower bound, so an upgrade changes no
existing XP, level or perfect day. 0014 adds the incremental statistics ledger and builds it
once per account from retained history before the server starts listening; it stores only
aggregates, deletes no history, and keeps the same creation-date bound so perfect days stay
exact. Phones keep working without an app update: the sync
generation fields are additive, and a request without them keeps the old behaviour.

**This source branch is not a deployment approval.** Routes, reminder settings and scheduler
are per-user here, but the upstream candidates and the rest of the multi-user stack still need
integration and independent acceptance. Migration 0007 adopts existing admin preferences.
Old experimental settings003 ledgers are deliberately unsupported: preserve the old file and
create a new empty path ONLY for a positively identified task-owned disposable experiment.
Never modify its ledger, recreate production, or remove reviewer evidence. See DESIGN.md.

### What can and cannot be undone

* Migrations are not reversible. An older image does not downgrade the file: its SQLx
  migrator reports the applied versions it does not know and the server exits instead of
  starting.
* **A failed migration does not restore the original database.** Each migration commits in
  its own transaction, so every migration that committed before the failure stays committed.
  In the failure this runbook was reviewed against, `0002` commits and `0005` fails. The
  database is left with the `0002` rebuild applied (one `users` row and every existing row
  owned by user 1) and `_sqlx_migrations` holding rows 1, 2 and 3, all with `success = 1`.
  There is no row for 5, and `PRAGMA user_version` is still 0 because SQLx records versions
  only in `_sqlx_migrations`. The server then refuses to start. The failed statement itself
  rolled back, so the file is a valid forward-repair point for this release, but it is not
  the pre-upgrade database: the old image cannot use it. Going back to the previous release
  always means restoring the full frozen copy from step 2, never "the database is unchanged".
  Fixing the cause and starting the new release again can complete the remaining migrations;
  that is forward repair, not rollback.
* **The copy is the recovery point, and writes after it are lost if you roll back.** The
  runbook stops every writer before the copy and does not start them again until step 4, so
  the maintenance window from step 2 to step 4 admits no new writes. Once the new release has
  served phones, rolling back loses every write accepted after the copy. Phones keep their
  local rows. This release reconciles by `updated_at`, treats a generation change or a server
  cursor behind the phone's cursor as a rollback, and then reoffers all local rows, but a
  rollback after live use can still discard a server-only write from that window, so use it
  only with explicit consent to that recovery point and data loss. Otherwise fix forward. A
  restored server can also have a lower `meta.seq` than a phone's stored `cursor`; that case
  is handled as a rollback by the same path and is covered by tests, not by a real device.
* The Litestream replica is a second copy, not the recovery point, and it is only trusted
  after the mandatory gate (step 3) shows it holds exactly the same data as the frozen copy.
  Its file names and modification times prove nothing about what is inside, so the gate
  compares the data itself. Only the frozen copy from step 2 is authoritative.

Everything below uses only what this repository ships: the pinned Litestream image (it has
`sh`, `cp`, `tar`) and `python3` on the host with its standard `sqlite3` module, driven by
`scripts/db-evidence.py` and `scripts/backup-gate.sh` (standard library only; the server image has no SQLite
command line). Run it as the ordinary deploy user, from the checkout, with the
default compose project name. Plan a short maintenance window: the app is unavailable between
step 2 and step 4 and phones retry on their own. Run the numbered blocks in one shell with
`set -e` (or chain each block with `&&`): the `test` and `mkdir` checks are written to fail
closed, not to be advisory.

1. **Record an immutable old image identifier and the data volume.** Docker tags are
   mutable, and `up --build` replaces `bread-tracker:local`. Record the current image's
   content-addressed ID in a new run directory and give the image a unique tag; do not reuse
   or overwrite an earlier run's tag:

   ```sh
   RUN_ID=$(date +%Y%m%d-%H%M%S)
   RUN=$HOME/bread-tracker-upgrade-$RUN_ID
   mkdir "$RUN"                                   # fails if it already exists
   chmod 700 "$RUN"
   PREV_IMAGE_ID=$(docker inspect --format '{{.Image}}' bread-tracker)
   test -n "$PREV_IMAGE_ID"
   test "$(docker image inspect --format '{{.Id}}' "$PREV_IMAGE_ID")" = "$PREV_IMAGE_ID"
   printf '%s\n' "$PREV_IMAGE_ID" > "$RUN/old-image-id"
   docker tag "$PREV_IMAGE_ID" "bread-tracker:pre-multiuser-$RUN_ID"
   printf '%s\n' "bread-tracker:pre-multiuser-$RUN_ID" > "$RUN/old-image-tag"
   VOL=$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/var/lib/bread-tracker"}}{{.Name}}{{end}}{{end}}' bread-tracker)
   test -n "$VOL"
   docker volume inspect "$VOL" --format '{{.Name}} {{.Driver}} {{.Mountpoint}}'
   echo "old image: $PREV_IMAGE_ID"
   echo "data volume: $VOL"
   ```

   `$PREV_IMAGE_ID` is the identifier used for rollback. The image ID is content-addressed
   and cannot be changed; the unique tag is only a convenience. `$RUN_ID` keeps this attempt's
   tag and backup directory distinct from every earlier attempt. The volume name must be
   non-empty and must be the volume compose mounts for this project (with the default project
   name and a checkout directory named `bread-tracker` it is
   `bread-tracker_bread-tracker-data`; confirm the name printed above). Stop if it is empty or
   unexpected.

2. **Freeze every writer and take the recovery copy.** Stop the application first, let
   Litestream finish shipping the last changes, then stop Litestream too. Both matter: the
   server writes the database and Litestream reads it and writes the replica. A copy taken
   from a running or merely quiet database is not a consistent SQLite snapshot; stopping both
   writers is the barrier, not the sleep. Then copy the stopped volume, including any `-wal`
   and `-shm` files, into this run's unique directory. The directory name is unique, so an
   earlier attempt can never be mistaken for this one:

   ```sh
   COPY=$RUN/volume
   mkdir "$COPY"                                   # fails if it already exists
   chmod 700 "$COPY"
   docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker
   sleep 10                                        # courtesy to Litestream, not a barrier
   docker compose -f packaging/compose.yaml --project-directory . stop litestream
   docker run --rm -v "$VOL":/data:ro -v "$COPY":/out \
     --entrypoint sh litestream/litestream:0.3.13 -c 'cp -a /data/. /out/'
   ls -l "$COPY"
   ```

   Do not start the containers again until step 4. Verify the copy itself. It must open,
   pass `integrity_check` and `foreign_key_check`, have a valid migration ledger (every
   migration recorded once and successful, with the checksum of the shipped file), and record
   only migrations this release supports. The embedded set is `1,2,3,4,5,6,7,8,9,10,11,12,13,14`:

   ```sh
   python3 scripts/db-evidence.py check "$COPY/bread-tracker.db" \
     --ledger-subset 1,2,3,4,5,6,7,8,9,10,11,12,13,14
   ```

   It prints the evidence (row counts, `meta.seq`, the migration ledger, a digest of every
   row) and ends with `CHECK PASSED`. The checker works on a private copy of the database file
   and of its `-wal` and `-shm` files, so the files it is pointed at, sidecars included, stay
   byte-identical. Anything else, including a recorded migration this release
   does not ship, is a stop: leave the copy as it is, go to "If the gate or
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

   Building replaces `bread-tracker:local`; the content-addressed ID saved in
   `$RUN/old-image-id` is now the only reliable reference to the old image. Do not delete
   `$RUN` or the old image until the new release has run well.

   The name is applied once: it takes effect only while the first user has never been named,
   and later restarts with the same or another value change nothing. If it is unset the user
   is called `admin` and can still be named by a later start. A name another user already
   holds stops startup with a message.

   This release ships migrations `1`, `2`, `3`, `4`, `5`, `6`, `7`, `8`, `9`, `10`, `11`,
   `12`, `13` and `14`. A database that has already recorded `4` (the reserved PR31 pairing-limiter
   version) is adopted by the same start: its checksum and table are kept and the later
   migrations apply.
   The live pairing and signup admission is migration `9`'s `public_auth_limits`; `4`'s
   `pair_rate_limits` table stays reserved and has no writer.

5. **Verify the result.** The new release and Litestream are now running. Give Litestream a
   few seconds, then restore the replica into a new scratch directory, as in step 3, and check
   it against the release's exact ledger:

   ```sh
   WORK=$(mktemp -d)
   docker run --rm --user "$(id -u):$(id -g)" -v "$BACKUP_DIR":/b:ro -v "$WORK":/o \
     litestream/litestream:0.3.13 restore -o /o/replica.db file:///b/litestream
   python3 scripts/db-evidence.py check "$WORK/replica.db" --ledger-exact 1,2,3,4,5,6,7,8,9,10,11,12,13,14
   ```

   Expect `CHECK PASSED` and the same counts for `items`, `completions`, `devices`,
   `reminders_sent` and `pairing_codes` as in step 2 (plus whatever phones synced since). The
   admin name is applied at start; confirm it in the server log or with the evidence output of
   a stopped copy of the volume.

### If the gate or the upgrade fails

*Before step 4 (the gate or the copy check failed).* The database has not been changed. Bring
the current release back and fix the cause; repeat from step 2 with a new `$COPY` once
replication is healthy (the old one can be kept or removed by you afterwards):

```sh
docker compose -f packaging/compose.yaml --project-directory . start bread-tracker litestream
docker logs bread-tracker-litestream | tail
```

*After step 4 (the upgrade failed or must be undone).* Stay in the maintenance window. This is
the only rollback path: restore the frozen copy over the data files, put the recorded old image
ID back under the name compose uses, and start without building. Do not repair the
post-failure database in place. If you are in a new shell, set `RUN` to the directory printed
in step 1, `COPY=$RUN/volume`, and `VOL` to the volume printed in step 1 first. Before deleting
anything, re-check the recovery point and the target volume:

```sh
test -s "$RUN/old-image-id" && PREV_IMAGE_ID=$(cat "$RUN/old-image-id")
test -n "$PREV_IMAGE_ID"
python3 scripts/db-evidence.py check "$COPY/bread-tracker.db" \
  --ledger-subset 1,2,3,4,5,6,7,8,9,10,11,12,13,14       # must end CHECK PASSED
VOL_NOW=$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/var/lib/bread-tracker"}}{{.Name}}{{end}}{{end}}' bread-tracker)
test "$VOL_NOW" = "$VOL"                         # same volume as the copy
docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker litestream
docker run --rm -v "$VOL":/data -v "$COPY":/in:ro \
  --entrypoint sh litestream/litestream:0.3.13 -c 'rm -f /data/bread-tracker.db* && cp -a /in/. /data/'
docker tag "$PREV_IMAGE_ID" bread-tracker:local
docker compose -f packaging/compose.yaml --project-directory . up -d --no-build
curl -s http://127.0.0.1:7290/healthz
```

Then stop the old image again and show the restored database's evidence from a fresh stopped
copy of the volume, as in step 2, and compare with the step 2 output: the same counts,
`meta.seq`, migrations and data digest. Do not point the checker at `$COPY` and call that the
restored result.

Keep `$RUN`, `$COPY` and `$PREV_IMAGE_ID` until the new release has run well. The sync
generation phones use is the separate `.sync-generation` sidecar rotated by `mark-restored`,
not Litestream's own generation. Litestream notices that the database file was replaced and
starts a new generation; confirm with `docker logs bread-tracker-litestream`.

### What this runbook has and has not verified

The source tests build the failed-migration state for real:
`server/tests/ledger.rs::a_failed_0005_keeps_the_0002_result_and_a_retry_completes_the_repair`
and the adjacent failing-0002/0006/0007 tests. Scratch evidence for this revision recorded
`PRAGMA user_version = 0`, ledger rows 1, 2 and 3 with `success = 1`, no row 5, the 0002
`user_id` rebuild present, and no 0005 `name_key` column. The frozen-copy checker was run on a
scratch copy, and the new image was built with the shipped Containerfile and started against a
scratch copy. That is source and component evidence only.

Not run for this change: the full Compose rollback above (stop, restore, retag,
`up --no-build`, health), restarting the old image on the restored volume, Litestream resuming
after the file replacement (it starts a new generation), and phone recovery after a restore.
They are operator deployment checks, not an approved rollback. A Litestream generation is the
replica's internal lineage, not a server identity or sync epoch. The server does record a
durable sync epoch and the app reconciles against it before uploading, but a full Compose
restart of the old image with Litestream resuming a generation, and phone recovery after a
restore, still need an operator run; they need no other pull request to be merged first. Do not
treat success of the copy, replica, new startup or database replacement as full-scope rollback
approval.

## Backup and restore

Litestream writes a file replica to `BREADTRACKER_BACKUP_DIR`. The sync-generation sidecar is deliberately stored outside SQLite, so an older database replica does not roll back the generation clients use to identify the server state. Every database restore must rotate that generation before the server accepts sync requests. If the sidecar survives a partial restore and the database cursor rewinds below a phone's cursor, the app treats that as a rollback and reoffers its clean rows instead of trusting the old cursor.

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
# Keep DB, WAL, SHM and candidate until replacement passes integrity_check. This image
# runs as root, so the replaced files stay usable without an ownership change.
docker run --rm --name "$TASK-replace" --user 0 \
  -v bread-tracker-data:/data -v bread-tracker-data:/var/lib/bread-tracker -v "$SCRATCH:/restore" \
  --entrypoint /bin/sh bread-tracker:local -ec '
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
  '
# Explicit ENTRYPOINT override is required: otherwise this command is appended to
# bread-tracker-server arguments instead of running the marker subcommand.
docker run --rm --name "$TASK-marker" \
  -v bread-tracker-data:/var/lib/bread-tracker \
  --entrypoint /usr/local/bin/bread-tracker-server bread-tracker:local mark-restored
docker compose -f packaging/compose.yaml --project-directory . up -d litestream
docker compose -f packaging/compose.yaml --project-directory . up -d bread-tracker
rm -rf "$SCRATCH"
```

If a marker fails after it has written `.sync-restore-failed`, the server and a new marker both refuse to run. Do not delete the database. Find the failing command's output, stop both services, and verify the restored database with

```sh
python3 scripts/db-evidence.py check "$SCRATCH/restored.db" --ledger-subset 1,2,3,4,5,6,7,8,9,10,11,12
```

The sidecar names are derived from the database path, not appended to the filename. For the default `BREADTRACKER_DATABASE_PATH=/var/lib/bread-tracker/bread-tracker.db` the derived files are `bread-tracker.sync-generation`, `bread-tracker.sync-generation-pending` and `bread-tracker.sync-restore-failed`. The server replaces the database suffix, so appending `sync-*` to the `.db` filename does not exist and `rm -f` there would silently leave the real sentinel in place.

Only after that check passes, remove the sentinel and the pending journal (if present) from the same volume, then rerun the marker against the same database path:

```sh
docker run --rm --name "$TASK-recover" \
  -v bread-tracker-data:/var/lib/bread-tracker \
  --entrypoint /bin/sh bread-tracker:local -ec '
    rm -f /var/lib/bread-tracker/bread-tracker.sync-restore-failed \
          /var/lib/bread-tracker/bread-tracker.sync-generation-pending'
docker run --rm --name "$TASK-marker" \
  -v bread-tracker-data:/var/lib/bread-tracker \
  --entrypoint /usr/local/bin/bread-tracker-server bread-tracker:local mark-restored
```

A pending journal alone is recoverable by normal startup, which rolls it forward; the sentinel is what stops that, so remove the sentinel last and only after the evidence check.

The normal operational restore above is intentionally separate from the disposable rehearsal below. This release keeps the tracker image on the root user so the existing root-owned volume opens without an ownership migration; the non-root image and its init-perms upgrade path belong to TRACK-15 / PR42. Stop both services before copying. Do not copy a generation sidecar from backup. The marker creates `.sync-restore-failed` before changing generation metadata and removes it only after success. Server startup and a subsequent marker invocation fail closed if the sentinel remains. Preserve the database, WAL and SHM until the restored candidate has passed integrity validation and replacement succeeds. The restore command is executed by the Litestream image's own default entrypoint; the replacement helper runs in the tracker image as root and leaves the data directory ownership unchanged. `pair` uses a short SQLite transaction and does not take the generation lock.

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

## Disposable restore rehearsal

Run the standalone rehearsal from a scratch checkout built from this revision. It uses only task-prefixed `--rm` containers, bind-mounted scratch directories and host port 17291. It does not use Compose or named volumes. Do not run it against production. The script prepares fixture files and the separate dummy secret inside root-run helpers, so it requires no host `chown` or `sudo`.

```sh
scripts/rehearse_restore.sh
```

The script checkpoints and integrity-checks the candidate, validates it before replacement, verifies marker and server startup, and tests secret readability without printing its contents. It uses a valid SQLx migration ledger with the SHA-384 checksum of the exact embedded 0001 migration. The server runs in the foreground under a host background PID: after health succeeds, the script verifies the exact task container name, stops only that container, and waits on the host `docker run` process for the exit status. Cleanup refuses to remove bind data while that container remains, uses a root helper to restore host uid/gid ownership, then removes only its exact scratch path and image. Runtime Docker verification is deliberately separate from static CI checks.
