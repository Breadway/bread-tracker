# Building and distributing the Android app

## Toolchain

`source scripts/android-env.sh` adds Flutter and the Android SDK to PATH and keeps an existing
`JAVA_HOME` (or uses `/usr/lib/jvm/java-17-openjdk` when that package is installed). For a
user-space JDK, export it first; adjust the version to what you installed:

```sh
export JAVA_HOME="$HOME/.local/share/java/jdk-17.0.21+9"
source scripts/android-env.sh
```

Then:

```sh
scripts/build-apk.sh debug
scripts/build-apk.sh release --name 1.1.0 --number 2
```

The APK is written to `app/build/app/outputs/flutter-apk/app-<mode>.apk`, and its SHA-256 is printed.

## Firebase config (google-services.json)

Push needs `app/android/app/google-services.json` (the Firebase client config for the app's
package, `dev.breadway.tracker`). It is not in the repo. Download it from the Firebase console
(Project settings, Your apps) and put it at that path; it is gitignored.

If the file is missing the build still succeeds (the google-services Gradle plugin is only applied
when the file exists) and prints a warning, but the app cannot initialise Firebase, so push
registration reports a failure in Settings. Everything else works offline and over sync.

For a CI runner, write the file from a secret before the build step, for example
`printf '%s' "$GOOGLE_SERVICES_JSON" > app/android/app/google-services.json`. `flutter analyze` and
`flutter test` do not need it.

## Release signing

A release build is signed with your own upload key and never with the debug key. Create a
keystore once and keep it (and its passwords) outside the repo and backed up: an APK signed with a
different key cannot be installed over an existing one.

```sh
keytool -genkeypair -v -keystore ~/secrets/bread-tracker-release.jks \
  -alias bread-tracker -keyalg RSA -keysize 2048 -validity 10000
```

Give Gradle the key either in `app/android/key.properties` (gitignored; copy
`key.properties.example`) or through environment variables, which suit CI:

| key.properties | environment variable |
| --- | --- |
| `storeFile` | `BREADTRACKER_KEYSTORE` |
| `storePassword` | `BREADTRACKER_KEYSTORE_PASSWORD` |
| `keyAlias` | `BREADTRACKER_KEY_ALIAS` |
| `keyPassword` | `BREADTRACKER_KEY_PASSWORD` |

key.properties wins over the environment. With any of the four missing, `assembleRelease` and
`bundleRelease` fail with a message saying so, rather than producing a debug-signed release.

To check what an APK was signed with (use the build-tools version you have):

```sh
"$ANDROID_HOME/build-tools/36.0.0/apksigner" verify --print-certs \
  app/build/app/outputs/flutter-apk/app-release.apk
```

The certificate must be your upload key, never `CN=Android Debug`.

## Versioning

The version is `version: X.Y.Z+N` in `app/pubspec.yaml`: `X.Y.Z` is the version name and `N` the
Android version code, which must increase with every APK you hand out or Android refuses the
update. `scripts/build-apk.sh release --name X.Y.Z --number N` overrides both for one build
without editing the file. Bump `pubspec.yaml` in the commit that you tag.

## Getting the APK onto a phone

There is no store listing. Either:

- `adb install -r app/build/app/outputs/flutter-apk/app-release.apk` over USB, or
- copy the APK to the phone (Nextcloud, a file share, a direct download link) and open it. Android
  asks to allow installing from that source once. Check the SHA-256 printed by the build script
  against the file you received.

Debug-signed APKs (`debug` mode) install the same way but can only be updated by another APK signed
with the same debug key, so use release builds for anyone other than a developer.
