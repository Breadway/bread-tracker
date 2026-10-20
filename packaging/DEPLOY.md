# Deploying bread-tracker on Hestia

```sh
rsync -a --delete --exclude target --exclude .git ./ hestia:~/bread-tracker/
ssh hestia 'cd ~/bread-tracker && mkdir -p /var/backups/bread-tracker 2>/dev/null; \
  docker compose -f packaging/compose.yaml --project-directory . up -d --build'
curl -s http://127.0.0.1:7290/healthz            # on Hestia: ok
docker exec bread-tracker bread-tracker-server pair   # one-time pairing code for the phone
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

Litestream writes a file replica to `BREADTRACKER_BACKUP_DIR`. Restore into a scratch file to verify:

```sh
docker run --rm -v /var/backups/bread-tracker:/b -v /tmp:/o litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b/litestream
sqlite3 /tmp/restored.db 'select count(*) from items;'
```

Run `packaging/restore-test.sh` to do this for the newest replica: it restores into a temporary
directory, runs `pragma integrity_check`, prints the item and device counts and exits non-zero on
failure. Schedule it (cron or a systemd timer) so a broken backup is noticed.

### Offsite copy

The Litestream replica above lives on the same host as the live database. It protects against a
corrupt database, not against losing the host or its disk. Copy `BREADTRACKER_BACKUP_DIR` to another
machine on a schedule, for example with `rsync -a --delete /var/backups/bread-tracker/ other-host:backups/bread-tracker/`
from a timer, or point a second Litestream replica at S3 compatible storage. No offsite target is
configured by this repository; choose one and run `restore-test.sh` against the copy too.

## Container user, healthcheck and base images

- The server runs as an unprivileged user (`tracker`, uid/gid 10001), the filesystem of the
  container stays writable only in the data volume, and the image has a `HEALTHCHECK` on `/healthz`
  (`docker ps` shows `healthy`).
- Upgrading from an image that ran as root: the existing data volume is owned by root. Compose runs a
  one-shot `init-perms` service that chowns the volume to `BREADTRACKER_UID:BREADTRACKER_GID`
  (default 10001) before the server starts, so no manual step is needed. The Litestream container runs
  as the same uid, so `BREADTRACKER_BACKUP_DIR` must be writable by it (`chown 10001 <dir>`).
- The FCM key file is mounted read-only as is and must be readable by that uid. If it is mode 600 and
  owned by you, either `chown 10001 <file>` or set `BREADTRACKER_UID`/`BREADTRACKER_GID` in `.env` to
  your own ids (and chown the backup directory to match).
- `rust:1-bookworm` and `debian:bookworm-slim` are pinned by digest in `packaging/Containerfile`. To take
  base image updates, run `docker buildx imagetools inspect <image>`, put the new digest in the
  `FROM` lines and rebuild.
