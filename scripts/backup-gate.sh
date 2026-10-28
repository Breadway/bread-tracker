#!/usr/bin/env bash
# Mandatory backup gate of the upgrade runbook (packaging/DEPLOY.md, step 3).
#
#   scripts/backup-gate.sh FROZEN_DB BACKUP_DIR
#
# Restores the Litestream replica found in BACKUP_DIR into a scratch file and compares it with
# the frozen copy FROZEN_DB. Exits 0 and prints "BACKUP GATE PASSED" only if the restore
# succeeds and scripts/db-evidence.py compare approves. In every other case (restore error,
# missing or empty replica, unreadable file, checker failure, bad arguments) it prints
# "BACKUP GATE FAILED: do not deploy" and exits NON-ZERO, so a script running under `set -e`
# or an `&&` chain cannot go on to deploy.
set -u

here=$(cd "$(dirname "$0")" && pwd)
fail() { echo "BACKUP GATE FAILED: do not deploy${1:+ ($1)}" >&2; echo "BACKUP GATE FAILED: do not deploy"; exit 1; }

[ "$#" -eq 2 ] || fail "usage: backup-gate.sh FROZEN_DB BACKUP_DIR"
frozen=$1
backup_dir=$2
[ -f "$frozen" ] || fail "frozen copy $frozen not found"
[ -d "$backup_dir" ] || fail "backup directory $backup_dir not found"
backup_dir=$(realpath "$backup_dir")

work=$(mktemp -d) || fail "cannot create a scratch directory"
trap 'rm -rf "$work"' EXIT

docker run --rm --user "$(id -u):$(id -g)" -v "$backup_dir":/b:ro -v "$work":/o \
  litestream/litestream:0.3.13 restore -o /o/replica.db file:///b/litestream \
  || fail "restore failed"

python3 "$here/db-evidence.py" compare "$frozen" "$work/replica.db" \
  || fail "replica does not match the frozen copy"

echo "BACKUP GATE PASSED"
