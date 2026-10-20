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

Rate limiting is per client address. Behind a reverse proxy the server only sees the proxy's address unless it is told to trust it. Set `BREADTRACKER_TRUSTED_PROXIES` to the address or CIDR the proxy connects from (comma separated, for example `100.64.0.0/10` for Tailscale) so `X-Forwarded-For` is used. Leave it empty and all callers share one bucket, which still only counts failed attempts. Check from the container that the address Aegis connects from matches before relying on it, and make sure the proxy overwrites rather than appends client-supplied `X-Forwarded-For`.

The compose file passes `BREADTRACKER_TRUSTED_PROXIES` through from `.env`.

## Push (FCM)

Put the Firebase service-account JSON on Hestia outside the repo (mode 600) and set in `.env`:

```
BREADTRACKER_FCM_KEY_FILE=/home/breadway/secrets/bread-tracker-fcm.json
BREADTRACKER_FCM_SERVICE_ACCOUNT=/run/secrets/fcm.json
```

Without it the server runs with push disabled (logged at startup).

Optional: `BREADTRACKER_PUSH_MODE=data` makes reminders data-only so the app can add a Mark done button to every one (default `notification`). Read app/NOTIFICATIONS.md and test on a phone before turning it on. The compose file passes it through from `.env`.

## Backup and restore

Litestream writes a file replica to `BREADTRACKER_BACKUP_DIR`. Restore into a scratch file to verify:

```sh
docker run --rm -v /var/backups/bread-tracker:/b -v /tmp:/o litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b/litestream
sqlite3 /tmp/restored.db 'select count(*) from items;'
```
