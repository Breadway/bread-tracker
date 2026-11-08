# Bread Tracker

A small self-hosted tracker for chores, habits and to-dos, with XP, levels, streaks and push
reminders. An offline-first Android app (Flutter) syncs to a small Rust server backed by SQLite.

- Chores and habits repeat on chosen weekdays; to-dos have a due date and are done once.
- Every completion earns XP, a "perfect day" earns a bonus, and XP turns into levels and ranks.
- Reminders are sent by the server through Firebase Cloud Messaging at each item's reminder time.
- The app works offline and syncs when it can. Conflicts are last write wins.

The rules are written down in [DESIGN.md](DESIGN.md). The XP rules are implemented twice (server and
app) and both are tested against the shared cases in `spec/xp_vectors.json`.

## Layout

| Path | What |
| --- | --- |
| `server/` | Rust server (axum, sqlx, SQLite): sync API, pairing, reminder scheduler, FCM sender |
| `migrations/` | SQLite schema |
| `app/` | Flutter Android app |
| `spec/` | Shared XP test vectors |
| `packaging/` | Containerfile, compose file, Litestream config, [DEPLOY.md](packaging/DEPLOY.md) |
| `scripts/` | Toolchain environment for the Android build |

## Run the server

```sh
cargo run -p bread-tracker-server
```

It listens on `127.0.0.1:7290` and keeps its data in `./bread-tracker.db`. Configuration is by
environment variable, all optional:

| Variable | Default | Meaning |
| --- | --- | --- |
| `BREADTRACKER_LISTEN` | `127.0.0.1:7290` | Address to listen on |
| `BREADTRACKER_DATABASE_PATH` | `bread-tracker.db` | SQLite file |
| `BREADTRACKER_FCM_SERVICE_ACCOUNT` | unset | Path to a Firebase service-account JSON. Push is disabled without it |
| `BREADTRACKER_REMINDER_GRACE_MINS` | `360` | How long after its time a reminder may still be sent |
| `BREADTRACKER_BUSY_TIMEOUT_MS` | `5000` | Milliseconds a connection waits for a locked SQLite database before reporting it as busy. Whole numbers from 1 to 600000; anything else stops startup |

The 5 second busy wait is the production default. Test suites that run many file-backed
writers rely on `BREADTRACKER_BUSY_TIMEOUT_MS` to stay reliable on a loaded host: the CI server
job sets 30000 ms so contention between parallel jobs cannot turn a correct assertion into a
`database is locked` error. No test retries an exclusivity failure.

## Pair a phone

Pairing links one device to the server. Print a one-time code (valid 15 minutes):

```sh
cargo run -p bread-tracker-server -- pair
# or, in the container: docker exec bread-tracker bread-tracker-server pair
```

Enter it in the app. The server returns a bearer token once and stores only its hash. When no device
is paired yet, the server also prints a code at startup.

## Join with an invite, or add a device

There are two ways to put the app on an account:

- **A new account.** The server owner makes a single-use invite code
  (`bread-tracker-server invite create`) and gives it to you with the server address. In the app
  choose **Join with an invite**, then enter the server, your name and the code.
- **Another device for an account you already have.** Ask the owner for a code
  (`bread-tracker-server pair --user <name>`), or on a phone that is already signed in use
  **Settings, Add another device**. On the new phone choose **Pair with a code**.

[docs/ONBOARDING.md](docs/ONBOARDING.md) covers installing the APK, joining, adding a second
device, signing out and deleting your account.

## Build the app

```sh
# Prerequisites: Flutter, JDK 17, and Android SDK command-line/platform tools.
# Set JAVA_HOME, ANDROID_SDK_ROOT (or ANDROID_HOME), and FLUTTER_ROOT as needed.
source scripts/android-env.sh    # adds available tool directories without replacing your configuration
# If both SDK variables are set differently, both explicit values are preserved;
# tool lookup uses ANDROID_SDK_ROOT first.
cd app
flutter pub get
flutter analyze
flutter test
flutter build apk --debug
```

The Android build needs `app/android/app/google-services.json` from your own Firebase project and the
app's package name `dev.breadway.tracker` (change the application id to use your own). It is
gitignored and not in this repository. The app's default server address is set in
`app/lib/data/store.dart`; it can be changed in the app's settings. See
[app/NOTIFICATIONS.md](app/NOTIFICATIONS.md) for push setup and the device checks that still need a
real phone.

## Test the server

```sh
cargo test --workspace --locked
```

## Deploy

See [packaging/DEPLOY.md](packaging/DEPLOY.md): a container with Litestream for SQLite replication,
and the optional Firebase push setup.

## License

MIT, see [LICENSE](LICENSE).
