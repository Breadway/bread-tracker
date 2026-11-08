#!/usr/bin/env bash
# Seed the pinned prebuilt sqlite3 native library into Flutter's hooks cache.
#
# `flutter test` runs the sqlite3 package's native-assets hook. That hook
# downloads libsqlite3.x64.linux.so from the sqlite3.dart GitHub release and
# verifies a pinned sha256. github.com is unreachable from the CI runner, so
# this script puts a known-good copy in the hook cache first. The hook still
# hashes an existing cache file and only reuses it when the hash matches, so a
# stale or corrupted committed file fails the test instead of passing.
#
# Run it after `flutter pub get` in app/: it reads the resolved sqlite3 package
# from app/.dart_tool/package_config.json to confirm the package version and
# the expected cache directory name before seeding anything.
#
# Usage: bash tools/ci/sqlite3/seed.sh [app-dir]
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
app_dir=${1:-"$repo_root/app"}
asset="$repo_root/tools/ci/sqlite3/libsqlite3.x64.linux.so"

# Pinned against sqlite3 3.5.2 (see README.md in this directory). A package
# bump changes the release tag and the asset hash, and with it the cache
# directory name, so these values must be updated together.
pinned_version=3.5.2
pinned_sha256=b17729184e5a2818055ecbddd5ed6642521bfe6e56aafa472330e483c0e2e0d2
pinned_dirname=download-b1772918

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

package_config="$app_dir/.dart_tool/package_config.json"
if [ ! -f "$package_config" ]; then
  fail "$package_config is missing; run 'flutter pub get' in $app_dir first"
fi

# Resolve the package from package_config.json rather than guessing a pub-cache
# path, then read the version and the pinned hash from the package source.
resolved=$(
  python3 - "$package_config" <<'PY'
import json
import os
import re
import sys
import urllib.parse

config_path = os.path.abspath(sys.argv[1])
with open(config_path, encoding="utf-8") as handle:
    config = json.load(handle)

package = next(
    (entry for entry in config.get("packages", []) if entry.get("name") == "sqlite3"),
    None,
)
if package is None:
    sys.exit("sqlite3 is not in %s" % config_path)

uri = package.get("rootUri", "")
parsed = urllib.parse.urlparse(uri)
if parsed.scheme == "file":
    root = urllib.parse.unquote(parsed.path)
elif parsed.scheme:
    sys.exit("sqlite3 rootUri has an unsupported scheme: %s" % uri)
else:
    root = os.path.normpath(
        os.path.join(os.path.dirname(config_path), urllib.parse.unquote(uri))
    )

version = None
with open(os.path.join(root, "pubspec.yaml"), encoding="utf-8") as handle:
    for line in handle:
        match = re.match(r"^version:\s*(\S+)\s*$", line)
        if match:
            version = match.group(1).strip("'\"")
            break
if version is None:
    sys.exit("could not read the version from %s/pubspec.yaml" % root)

hashes_path = os.path.join(root, "lib", "src", "hook", "asset_hashes.dart")
with open(hashes_path, encoding="utf-8") as handle:
    hashes = handle.read()
match = re.search(
    r"'libsqlite3\.x64\.linux\.so'\s*:\s*'([0-9a-f]{64})'",
    hashes,
)
if match is None:
    sys.exit("no pinned hash for libsqlite3.x64.linux.so in %s" % hashes_path)

print(version)
print(match.group(1))
PY
)
resolved_version=$(sed -n 1p <<<"$resolved")
resolved_sha256=$(sed -n 2p <<<"$resolved")

if [ "$resolved_version" != "$pinned_version" ]; then
  fail "the committed asset is pinned to sqlite3 $pinned_version but the app resolves $resolved_version." \
    "Download the $resolved_version artifact and update the pin (see tools/ci/sqlite3/README.md)."
fi
if [ "$resolved_sha256" != "$pinned_sha256" ]; then
  fail "sqlite3 $resolved_version pins libsqlite3.x64.linux.so to $resolved_sha256," \
    "but this repository seeds $pinned_sha256. Replace the committed asset and pin together."
fi
resolved_dirname="download-${resolved_sha256:0:8}"
if [ "$resolved_dirname" != "$pinned_dirname" ]; then
  fail "sqlite3 $resolved_version expects the cache directory $resolved_dirname," \
    "but this script seeds $pinned_dirname. Replace the committed asset and pin together."
fi

if [ ! -f "$asset" ]; then
  fail "$asset is missing"
fi
actual_sha256=$(sha256sum "$asset" | cut -d' ' -f1)
if [ "$actual_sha256" != "$pinned_sha256" ]; then
  fail "$asset hashes to $actual_sha256, expected $pinned_sha256." \
    "The committed binary is corrupt or was replaced without updating the pin."
fi

seed_dir="$app_dir/.dart_tool/hooks_runner/shared/sqlite3/build/$pinned_dirname"
mkdir -p "$seed_dir"
cp "$asset" "$seed_dir/libsqlite3.so"
seeded_sha256=$(sha256sum "$seed_dir/libsqlite3.so" | cut -d' ' -f1)
if [ "$seeded_sha256" != "$pinned_sha256" ]; then
  fail "the seeded file at $seed_dir/libsqlite3.so hashes to $seeded_sha256"
fi

echo "seeded $seed_dir/libsqlite3.so (sha256 $pinned_sha256, sqlite3 $pinned_version)"
