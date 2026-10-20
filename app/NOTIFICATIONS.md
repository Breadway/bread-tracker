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
Firebase project `bread-tracker-df441`, package `dev.breadway.tracker`. This client
configuration is not the server service-account private key. Never put the
service-account key in the app or repository.

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

## Mark done and tapping a reminder

Tapping a reminder opens the app on the Today screen for the reminded day. The
**Mark done** action completes the item without opening the app: it writes the
completion to the local database from a background isolate and then tries to
sync it. Offline, it syncs the next time the app runs.

Which notifications have the action depends on the server's
`BREADTRACKER_PUSH_MODE` (see DESIGN.md):

- `notification` (default): Android shows the reminder itself. No action button
  while the app is closed or in the background; tapping opens the app. While
  the app is in the foreground it shows its own notification with Mark done.
- `data`: the app builds every reminder, so each has Mark done even when the app
  is closed. Switch to this only after checking the acceptance list below on a
  device, because delivery depends on Android allowing the background handler.

Repeat reminders, overdue to-dos and snooze are not implemented.

## Device acceptance checks still required

- Test delivery with the app foregrounded, backgrounded, and swiped away.
- Deny notification permission, then enable it in Android settings and retry.
- Pair without notification permission, then enable it and confirm registration.
- Complete an item offline and verify sync after reconnecting.
- Confirm scheduled reminders use Perth time and skip completed items.
- With `BREADTRACKER_PUSH_MODE=data`: reminders arrive and show Mark done with
  the app foregrounded, backgrounded, swiped away and after a reboot; check
  battery optimisation settings on the phone.
- Tap Mark done offline, then reconnect and confirm the completion syncs and the
  item shows as done in the app (and no later reminder is sent for it).
- Tap Mark done twice, and on an item deleted meanwhile; nothing should change.
- Tap a reminder with the app closed, backgrounded and foregrounded; it should
  land on Today for the reminded day.
- A release (R8) build may need keep rules for flutter_local_notifications; test
  the release APK, not only debug.
- Test under Doze. Android force-stop is different from swiping away and can
  prevent delivery until the app is opened again.
