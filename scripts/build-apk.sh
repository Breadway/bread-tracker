#!/usr/bin/env bash
# Build the Android APK.
#   scripts/build-apk.sh debug
#   scripts/build-apk.sh release [--name 1.2.0] [--number 7]
# Release builds need signing config (android/key.properties or BREADTRACKER_* env vars) and
# refuse to fall back to the debug key. See app/RELEASE.md.
set -euo pipefail

mode="${1:-}"
[ "$mode" = debug ] || [ "$mode" = release ] || { echo "usage: $0 debug|release [--name X.Y.Z] [--number N]" >&2; exit 2; }
shift

name="" number=""
while [ $# -gt 0 ]; do
  case "$1" in
    --name) name="$2"; shift 2 ;;
    --number) number="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=android-env.sh
source "$root/scripts/android-env.sh"
cd "$root/app"

if [ ! -f android/app/google-services.json ]; then
  echo "warning: app/android/app/google-services.json is missing: the APK will build but push notifications will not work" >&2
fi

args=(--"$mode")
[ -n "$name" ] && args+=(--build-name "$name")
[ -n "$number" ] && args+=(--build-number "$number")

flutter pub get
flutter build apk "${args[@]}"
apk="build/app/outputs/flutter-apk/app-$mode.apk"
echo "built $PWD/$apk"
sha256sum "$apk"
