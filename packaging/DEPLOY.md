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

Hestia's own Caddy (LAN) and Aegis (public) both reverse-proxy `tracker.breadway.dev` to port 7290; the blocks live in the infra repo (`hestia/Caddyfile`, `aegis/Caddyfile`). Aegis reaches Hestia over Tailscale, so bind only the Tailscale interface address, for example `BREADTRACKER_BIND=<Hestia-Tailscale-IP>:7290` in `.env`. Do not use `0.0.0.0`: that also exposes the plain HTTP listener on every LAN interface. Confirm the selected address with the operator's Tailscale configuration before deployment.

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

Install `restore-test.service.example` and `restore-test.timer.example` as systemd user or
system units after setting the correct project directory and confirming Docker/sqlite3 availability.
The schedule is daily and persistent; it verifies the local replica without modifying live data.

Run `packaging/restore-test.sh` to do this for the newest replica: it restores into a temporary
directory, runs `pragma integrity_check`, prints the item and device counts and exits non-zero on
failure. `restore-test.service.example` and `restore-test.timer.example` provide a daily,
persistent systemd schedule. Install only after confirming the replica path and dependencies.

### Offsite copy

The Litestream replica above lives on the same host as the live database. It protects against a
corrupt database, not against losing the host or its disk. Use `packaging/offsite-backup.sh` with
`BREADTRACKER_OFFSITE_SOURCE` and `BREADTRACKER_OFFSITE_DEST` configured in an operator-managed
environment file. It uses rsync and refuses to run until both are explicitly supplied. Example
hourly service/timer files are provided. Choose and secure the SSH/S3 destination and credentials
outside this repository; no destination is invented here. Run `restore-test.sh` against the copy too.

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
