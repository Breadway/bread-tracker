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

This release adds migrations `0002_users.sql` and `0005_users_forward_repair.sql`, applied
automatically at server start. 0002 adds a `users` table, gives all existing data to the first
user (the admin) and rebuilds `items`, `completions`, `devices`, `reminders_sent` and
`pairing_codes` with a `user_id`. 0005 restores the scheduler indexes the rebuild drops and
adds a name key. Phones keep working without an app update, the sync API did not change.
Migrations are not reversible by starting an older image: the way back is the backup in step 2.

**This release is not multi-user yet.** Only the first user's devices can authenticate and all
routes read that user's data. Do not create other users until the per-user routing and settings
changes are released. See "Users and migrations" in DESIGN.md.

Everything below uses only the images and tools this repository already ships: the pinned
Litestream image and `python3` on the host (its standard `sqlite3` module; the server image has
no SQLite command line). Run it as the ordinary deploy user, from the checkout, with the
default compose project name.

1. Verify the Litestream replica is current and restorable, and note the counts:

   ```sh
   BACKUP_DIR=/var/backups/bread-tracker        # your BREADTRACKER_BACKUP_DIR
   WORK=$(mktemp -d)
   docker run --rm -v "$BACKUP_DIR":/b:ro -v "$WORK":/o litestream/litestream:0.3.13 \
     restore -o /o/replica.db file:///b/litestream
   python3 - "$WORK/replica.db" <<'PY'
   import sqlite3, sys
   c = sqlite3.connect(sys.argv[1])
   for t in ("items", "completions", "devices", "reminders_sent", "pairing_codes"):
       print(t, c.execute(f"select count(*) from {t}").fetchone()[0])
   print("migrations", [r[0] for r in c.execute("select version from _sqlx_migrations order by version")])
   PY
   ```

2. Take an explicit copy that you control. Stop the writers so the copy is consistent, copy the
   volume, start again (a few seconds of downtime; phones retry on their own):

   ```sh
   mkdir -p ~/bread-tracker-pre-upgrade
   docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker litestream
   docker run --rm -v bread-tracker_bread-tracker-data:/data:ro -v ~/bread-tracker-pre-upgrade:/out \
     --entrypoint sh litestream/litestream:0.3.13 -c 'cp -a /data/. /out/'
   docker compose -f packaging/compose.yaml --project-directory . start bread-tracker litestream
   ls -l ~/bread-tracker-pre-upgrade
   ```

   (`bread-tracker_bread-tracker-data` is the compose project volume; check the name with
   `docker volume ls`.) Keep the copy until you are happy with the result.

3. Name the first user by setting `BREADTRACKER_ADMIN_NAME` in `.env` (for example `Riley`),
   then deploy as usual (`docker compose ... up -d --build`). The name is applied once: it takes
   effect only while the first user has never been named, and later restarts, with the same or
   another value, change nothing. If you leave it unset the user is called `admin` and can still
   be named by a later start. A name another user already holds stops startup with a message.

4. Check the result. The server logs show no migration error and `/healthz` answers `ok`. Copy
   the live database out of the stopped-or-quiet service the same way as step 2 (or restore the
   fresh Litestream replica as in step 1) and compare: the five counts must equal step 1 plus
   any changes made since, `select id, name, is_admin from users` must show one admin, and
   `select version from _sqlx_migrations` must list 1, 2, 3 (and 4 once it ships) and 5.

If a migration fails the server refuses to start and the database is unchanged: 0002 and 0005
each run in their own transaction. To go back to the old release after a successful upgrade,
stop the service, replace the volume contents with the step 2 copy, and start the previous
image:

```sh
docker compose -f packaging/compose.yaml --project-directory . stop bread-tracker litestream
docker run --rm -v bread-tracker_bread-tracker-data:/data -v ~/bread-tracker-pre-upgrade:/in:ro \
  --entrypoint sh litestream/litestream:0.3.13 -c 'rm -f /data/bread-tracker.db* && cp -a /in/. /data/'
```

then start the previous image. Litestream must be told to start a new generation after a
restore, which it does by itself when it finds the database changed.
