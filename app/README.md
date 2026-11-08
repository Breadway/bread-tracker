# Bread Tracker app

The Flutter Android client for Bread Tracker. See the [repository README](../README.md) for what it
is and how the server works, [NOTIFICATIONS.md](NOTIFICATIONS.md) for push setup and device checks,
and [../DESIGN.md](../DESIGN.md) for the rules and sync model.

```sh
source ../scripts/android-env.sh
flutter pub get
flutter analyze
flutter test
```

Building an APK needs `android/app/google-services.json`, which is not in the repository.
