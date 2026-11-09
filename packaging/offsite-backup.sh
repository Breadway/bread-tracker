#!/usr/bin/env bash
# Copy a local Litestream replica to an operator-selected destination. Intentionally exits
# until both paths are explicitly configured; never guesses or embeds a remote host/credential.
set -euo pipefail
: "${BREADTRACKER_OFFSITE_SOURCE:?Set BREADTRACKER_OFFSITE_SOURCE to the local Litestream replica directory}"
: "${BREADTRACKER_OFFSITE_DEST:?Set BREADTRACKER_OFFSITE_DEST to a configured rsync destination}"
[[ -d "$BREADTRACKER_OFFSITE_SOURCE" ]] || { echo "source replica directory is missing" >&2; exit 1; }
command -v rsync >/dev/null || { echo "rsync is required" >&2; exit 1; }
rsync -a --delete --partial --delay-updates \
  "${BREADTRACKER_OFFSITE_SOURCE%/}/" "${BREADTRACKER_OFFSITE_DEST%/}/"
