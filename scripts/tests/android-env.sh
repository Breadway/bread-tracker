#!/usr/bin/env bash
set -euo pipefail
script=$(cd "$(dirname "$0")/.." && pwd)/android-env.sh

run_case() {
  local name=$1 home=$2 home_var=$3 root_var=$4 expected_home=$5 expected_root=$6
  env -i HOME="$home" PATH=/usr/bin:/bin CASE_HOME="$home_var" CASE_ROOT="$root_var" EXPECT_HOME="$expected_home" EXPECT_ROOT="$expected_root" SCRIPT="$script" bash -c '
    if [[ $CASE_HOME != __unset__ ]]; then export ANDROID_HOME="$CASE_HOME"; fi
    if [[ $CASE_ROOT != __unset__ ]]; then export ANDROID_SDK_ROOT="$CASE_ROOT"; fi
    source "$SCRIPT"
    [[ ${ANDROID_HOME-__unset__} == "$EXPECT_HOME" ]]
    [[ ${ANDROID_SDK_ROOT-__unset__} == "$EXPECT_ROOT" ]]
  ' || { echo "failed: $name" >&2; return 1; }
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/home/Android" "$tmp/selected-sdk" "$tmp/other-sdk"
run_case root-selected "$tmp/home" __unset__ "$tmp/selected-sdk" "$tmp/selected-sdk" "$tmp/selected-sdk"
run_case home-only "$tmp/home" "$tmp/other-sdk" __unset__ "$tmp/other-sdk" "$tmp/other-sdk"
run_case both-explicit "$tmp/home" "$tmp/other-sdk" "$tmp/selected-sdk" "$tmp/other-sdk" "$tmp/selected-sdk"
run_case default "$tmp/home" __unset__ __unset__ "$tmp/home/Android" "$tmp/home/Android"
run_case neither "$tmp/empty-home" __unset__ __unset__ __unset__ __unset__
run_case no-java-sdk "$tmp/empty-home" __unset__ __unset__ __unset__ __unset__
echo 'android-env tests passed'
