# Notification development and acceptance

## Build and check

From the repository root:

```sh
source scripts/android-env.sh
cd app
flutter pub get
flutter analyze
flutter test
flutter build apk --debug
```

The APK is `app/build/app/outputs/flutter-apk/app-debug.apk` relative to the
repository root. It is a debug build, not a signed production release.

Android builds require the gitignored `android/app/google-services.json` for
the operator's Firebase project configured for the app package name
`dev.breadway.tracker`. This client configuration is not the server
service-account private key. Never put the service-account key in the app or
repository.

## Pairing and notifications

1. Install the APK on an authorized phone using `adb install -r <apk>`.
2. Generate a pairing code on the server using
   `docker exec bread-tracker bread-tracker-server pair`.
3. Enter the code in the app. Offline use is also available before pairing.
4. In Settings, tap the notification status row to request permission or retry
   registration. If Android has blocked permission, allow notifications in the
   phone's app settings first.
5. Use **Send a test notification**. This first registers the current device
   target with the server. Registration alone does not prove delivery: the
   server also needs its Firebase service-account key configured as described in
   `packaging/DEPLOY.md` at the repository root.

Registration is retried when the app resumes or connectivity returns. Startup
does not wait for Firebase or prompt for notification permission. The user is
prompted when pairing or explicitly enabling/testing notifications.

The current server sends standard FCM notification messages. Android displays
them in the background, and the app displays a local notification in the
foreground. **Mark done from a notification is not implemented yet.**

## Device acceptance checks still required

- Test delivery with the app foregrounded, backgrounded, and swiped away.
- Deny notification permission, then enable it in Android settings and retry.
- Pair without notification permission, then enable it and confirm registration.
- Complete an item offline and verify sync after reconnecting.
- Confirm scheduled reminders use the server's configured timezone and skip completed items.
- Test under Doze. Android force-stop is different from swiping away and can
  prevent delivery until the app is opened again.
