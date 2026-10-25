# Deploying bread-tracker

```sh
rsync -a --delete --exclude target --exclude .git ./ your-server:~/bread-tracker/
ssh your-server 'cd ~/bread-tracker && mkdir -p /var/backups/bread-tracker 2>/dev/null; \
  docker compose -f packaging/compose.yaml --project-directory . up -d --build'
curl -s http://127.0.0.1:7290/healthz            # on the server: ok
docker exec bread-tracker bread-tracker-server pair   # one-time pairing code for the phone
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

Litestream writes a file replica to `BREADTRACKER_BACKUP_DIR`. Restore into a scratch file to verify:

```sh
docker run --rm -v /var/backups/bread-tracker:/b -v /tmp:/o litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b/litestream
sqlite3 /tmp/restored.db 'select count(*) from items;'
```

## Upgrading an existing single-user deployment to the multi-user schema

This release adds migrations `0002_users.sql`, `0005_users_forward_repair.sql` and
`0006_users_name_key_guard.sql`, applied automatically at server start, each in its own
transaction. 0002 adds a `users` table, gives all existing data to the first user (the admin)
and rebuilds `items`, `completions`, `devices`, `reminders_sent` and `pairing_codes` with a
`user_id`. 0005 restores the scheduler indexes the rebuild drops and adds a name key. 0006
tightens the name-key guard. Phones keep working without an app update: the sync API did not
change.

**This release is not multi-user yet.** Only the first user's devices can authenticate, all
routes read that user's data, and the background reminder scheduler serves that user alone.
Do not create other users until the per-user routing, settings and scheduling changes are
released. See "Users and migrations" in DESIGN.md.

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
* The Litestream replica is a second copy, not the recovery point, and its counts alone do not
  show that it is current. Only the frozen copy from step 2 is authoritative.

Everything below uses only what this repository ships: the pinned Litestream image (it has
`sh`, `cp`, `tar`) and `python3` on the host with its standard `sqlite3` module (the server image
has no SQLite command line). Run it as the ordinary deploy user, from the checkout, with the
default compose project name. Plan a short maintenance window: the app is unavailable between
step 2 and step 5 and phones retry on their own.

1. **Record what is running.** Pin the current image under a name that building cannot
   overwrite (`up --build` replaces `bread-tracker:local`), and find the data volume:

   ```sh
   docker tag "$(docker inspect --format '{{.Image}}' bread-tracker)" bread-tracker:pre-multiuser
   docker image inspect --format '{{.Id}}' bread-tracker:pre-multiuser    # note this id
   VOL=$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/var/lib/bread-tracker"}}{{.Name}}{{end}}{{end}}' bread-tracker)
   echo "data volume: $VOL"                                               # must not be empty
   ```

2. **Freeze writers and take the recovery copy.** Stop the server and Litestream first; a
   copy of a live database file is not a backup. The directory name is unique, so an earlier
   attempt can never be mistaken for this one:

   ```sh
   COPY=$HOME/bread-tracker-pre-multiuser-$(date +%Y%m%d-%H%M%S)
   mkdir "$COPY"
   docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker litestream
   docker run --rm -v "$VOL":/data:ro -v "$COPY":/out \
     --entrypoint sh litestream/litestream:0.3.13 -c 'cp -a /data/. /out/'
   ls -l "$COPY"
   ```

   Do not start the containers again until step 5. Verify the copy itself, and note the
   counts and the last sequence number:

   ```sh
   python3 - "$COPY/bread-tracker.db" <<'PY'
   import sqlite3, sys
   c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
   print("integrity", c.execute("pragma integrity_check").fetchone()[0])
   print("foreign keys", c.execute("pragma foreign_key_check").fetchall())
   for t in ("items", "completions", "devices", "reminders_sent", "pairing_codes"):
       print(t, c.execute(f"select count(*) from {t}").fetchone()[0])
   print("meta.seq", c.execute("select val from meta where key = 'seq'").fetchone()[0])
   print("migrations", [r[0] for r in c.execute("select version from _sqlx_migrations order by version")])
   PY
   ```

   `integrity` must be `ok` and `foreign keys` empty. Opening a copy that has a `-wal` file
   next to it is read-only here, so the copy is not modified.

3. **(Optional) Check the replica against the frozen copy.** The replica is current only if it
   restores to the same `meta.seq` and counts as the frozen copy:

   ```sh
   WORK=$(mktemp -d)
   docker run --rm -v /var/backups/bread-tracker:/b:ro -v "$WORK":/o litestream/litestream:0.3.13 \
     restore -o /o/replica.db file:///b/litestream    # your BREADTRACKER_BACKUP_DIR
   ```

   then run the same Python block on `$WORK/replica.db` and compare it with step 2.

4. **Name the first user and deploy.** Set `BREADTRACKER_ADMIN_NAME` in `.env` (for example
   `Riley`), then build and start the new release:

   ```sh
   docker compose -f packaging/compose.yaml --project-directory . up -d --build
   curl -s http://127.0.0.1:7290/healthz        # ok (use your BREADTRACKER_BIND)
   docker logs bread-tracker | tail             # no migration error
   ```

   The name is applied once: it takes effect only while the first user has never been named,
   and later restarts with the same or another value change nothing. If it is unset the user
   is called `admin` and can still be named by a later start. A name another user already
   holds stops startup with a message. A database that has recorded a migration this release
   does not ship (for example `0004` from the durable pairing limiter) is refused.

5. **Verify the result.** Give Litestream a few seconds, restore the replica as in step 3
   and run the same Python block on it. Expect: `integrity ok`, empty `foreign keys`, the same
   five counts as step 2 (plus whatever phones synced since), `select id, name, is_admin from
   users` showing one admin, and migrations `1, 2, 3, 5, 6` (and `4` if it was already there).

### If the upgrade fails or must be undone

Stay in the maintenance window: leave the writers stopped, then restore the frozen copy and
the previous image. Replace the data files, put the pinned image back under the name compose
uses, and start without building:

```sh
docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker litestream
docker run --rm -v "$VOL":/data -v "$COPY":/in:ro \
  --entrypoint sh litestream/litestream:0.3.13 -c 'rm -f /data/bread-tracker.db* && cp -a /in/. /data/'
docker tag bread-tracker:pre-multiuser bread-tracker:local
docker compose -f packaging/compose.yaml --project-directory . up -d --no-build
curl -s http://127.0.0.1:7290/healthz
```

Then repeat the Python check on the restored copy: counts and `meta.seq` equal to step 2. Keep
`$COPY` and `bread-tracker:pre-multiuser` until the new release has run well. Litestream
notices that the database file was replaced and begins a new generation; confirm with
`docker logs bread-tracker-litestream`. A Litestream generation is not the sync-epoch
mechanism for phones, which this release does not ship.
