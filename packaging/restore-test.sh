#!/usr/bin/env bash
# Restore the newest Litestream replica into a scratch file and sanity-check it. Run it
# periodically (cron or a systemd timer); a non-zero exit means the backup cannot be restored.
#
#   packaging/restore-test.sh [replica-dir]     optional explicit Litestream replica directory
#   BREADTRACKER_BACKUP_DIR sets backup-dir, default /var/backups/bread-tracker
#
# Needs docker and sqlite3 on the host. Writes only to a temporary directory.
set -euo pipefail

backup_dir="${BREADTRACKER_BACKUP_DIR:-/var/backups/bread-tracker}"
replica_dir="${1:-${BREADTRACKER_RESTORE_REPLICA:-$backup_dir/litestream}}"
[ -d "$replica_dir" ] || { echo "no replica at $replica_dir" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
chmod 777 "$tmp"

docker run --rm --user "$(id -u):$(id -g)" \
  -v "$replica_dir:/b:ro" -v "$tmp:/o" litestream/litestream:0.3.13 \
  restore -o /o/restored.db file:///b

integrity="$(sqlite3 "$tmp/restored.db" 'pragma integrity_check;')"
[ "$integrity" = ok ] || { echo "integrity_check failed: $integrity" >&2; exit 1; }
items="$(sqlite3 "$tmp/restored.db" 'select count(*) from items;')"
devices="$(sqlite3 "$tmp/restored.db" 'select count(*) from devices;')"
completions="$(sqlite3 "$tmp/restored.db" 'select count(*) from completions;')"
sqlite3 "$tmp/restored.db" "select count(*) from sqlite_master where type='table' and name in ('items','completions','devices','meta');" | grep -qx 4 || { echo "required application tables are missing" >&2; exit 1; }
echo "restore ok: $items items, $completions completions, $devices devices"
