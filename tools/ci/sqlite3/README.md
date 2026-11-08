# Pinned sqlite3 native library for CI

`libsqlite3.x64.linux.so` is the prebuilt SQLite shared library for x86-64
Linux that the `sqlite3` Dart package normally downloads at build time.

## Why it is committed

`flutter test` runs the `sqlite3` package's native-assets hook. For the
default `source: precompiled` configuration the hook downloads
`libsqlite3.x64.linux.so` from the package's GitHub release and verifies a
pinned sha256. The CI runner cannot reach `github.com` (it returns a persistent
504), so the hook writes a small HTML error page instead of a library and the
test job fails before any test runs.

Committing the known-good library lets `tools/ci/sqlite3/seed.sh` place it in
the hook cache before the test step. The hook still hashes a cache file and
reuses it only when the hash matches the pin, so a stale or corrupted commit
cannot silently pass. This changes nothing about how the app sources SQLite at
runtime: the hook consumes the same library it would have downloaded.

## Where it came from

- Package: `sqlite3` 3.5.2 (`app/pubspec.lock`)
- Source URL:
  `https://github.com/simolus3/sqlite3.dart/releases/download/sqlite3-3.5.2/libsqlite3.x64.linux.so`
- sha256:
  `b17729184e5a2818055ecbddd5ed6642521bfe6e56aafa472330e483c0e2e0d2`
- Size: 1801592 bytes
- File type: ELF 64-bit LSB shared object, x86-64

The hook derives its cache directory from the first eight hex digits of that
hash, which is why the pinned path is
`app/.dart_tool/hooks_runner/shared/sqlite3/build/download-b1772918/libsqlite3.so`.

## When it must be replaced

Replace the binary, the hash and the directory name together whenever:

- the `sqlite3` package is upgraded (`app/pubspec.lock`), because the release
  tag and the pinned hash in the package's `lib/src/hook/asset_hashes.dart`
  change, and with them the `download-<first 8 hex>` directory name;
- the artifact for a different OS or architecture is needed (the current file
  only covers x86-64 Linux).

`seed.sh` reads the resolved package version and hash from
`app/.dart_tool/package_config.json` and the package source, and fails loudly
when either the version, the hash or the expected directory name no longer
matches this pin. To update:

1. On a host that can reach `github.com`, download the artifact for the new
   release tag and record its sha256.
2. Replace `libsqlite3.x64.linux.so` with the downloaded file.
3. Update `pinned_version`, `pinned_sha256` and `pinned_dirname` in
   `seed.sh`, and this README.
4. Run `bash tools/ci/sqlite3/seed.sh` after `flutter pub get` in `app/` to
   confirm the hash and directory name are accepted.

## Local use

```sh
cd app && flutter pub get
cd .. && bash tools/ci/sqlite3/seed.sh
cd app && flutter test
```

The script only writes under `app/.dart_tool/`, which is ignored by git, so it
is safe to run in a normal checkout.
