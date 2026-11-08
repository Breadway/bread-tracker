# Joining a Bread Tracker server

Bread Tracker is a chore, habit and to-do tracker with an Android app. This page is for someone
who has been invited to a server run by somebody else. If you run the server yourself, see
[packaging/DEPLOY.md](../packaging/DEPLOY.md) and [DESIGN.md](../DESIGN.md).

## What you should know first

- You get your **own account**. Your items, history, streaks, settings and reminders are private
  to you: other users of the same server cannot see or change them, and you cannot see theirs.
- **The person who runs the server can read everything stored on it.** That includes your name,
  your items and ticks, your settings and the names of your devices. Nothing is encrypted per user,
  and they also control the machine and its backups. Only join a server whose owner you trust with
  that. Your device tokens and your invite code are stored only as hashes.
- You can leave at any time: **Settings, Export my data** copies everything to your clipboard as
  JSON, and **Delete my account** removes your account and data from the server (type your name to
  confirm). Backups the server owner already took are not rewritten, so copies can survive in them
  until they rotate out.
- There is a limit on how many accounts the server accepts, how many items and history entries each
  account may hold, how many devices you can have, and how fast the app may call the server. Normal
  use is far below these. If you do hit one, Settings shows the server's own message (for
  example that the item limit was reached) and the app keeps your local changes; ask the server
  owner if it is unclear, since they can also see it in the server log.

## What you need

1. The server address, for example `https://tracker.example.com`, from the server owner.
2. An **invite code** from the server owner, like `ABCD-EFGH-JKLM`. It works once and expires
   (7 days unless they chose otherwise). Ask for a new one if it has expired or was used.
3. The app, as an APK (see below).

## Installing the app

There is no store listing, so the app is an APK file that you install yourself:

1. Get the APK from the server owner, or build it from the repository (see `app/NOTIFICATIONS.md`
   for the build commands).
2. Copy it to your phone and open it, or run `adb install -r app-release.apk` from a computer.
   Android asks once to allow installing from that source.
3. **Important:** unless the server owner gives you a release build, the app is a
   **debug-signed APK** (release signing is a separate, not yet merged change). It installs and works, but an APK signed with a different key cannot be
   installed over it: updating later means uninstalling first, which deletes the data stored on the
   phone (your account and data on the server are unaffected, you just sign in again). Use a release
   build if the owner offers one.
4. Push notifications only work if the APK was built with the server owner's Firebase
   configuration. Without it everything else works, but you will not get reminders, and Settings
   shows that notification registration failed.

## Joining with an invite

1. Open the app. On the first screen choose **Join with an invite**.
2. Check the **Server** field: it is pre-filled with the default server and you can change it.
3. Choose a **name** (1 to 40 letters, digits, spaces, dots, underscores or dashes; it must not be
   taken by someone else, ignoring case).
4. Enter the **invite code** and tap **Join**.
5. The app sends your phone's timezone so reminders fire at the right local time. If it cannot
   read it, your account starts on UTC; change it in **Settings, Reminders**.
6. Allow notifications when asked.

If something goes wrong:

| Message | What to do |
| --- | --- |
| Invite code wrong, already used or expired | Ask for a new code. |
| That name is already taken | Choose a different name; the invite is not used up. |
| Too many wrong attempts | Wait about 10 minutes. |
| Cannot reach the server | Check the address and your connection. |
| Server is too old for this version of the app | The server owner must update the server. |

If your phone already holds data from **another account or server**, the app warns you that
joining deletes it from the phone first (so it never mixes into your new account) and asks you to
confirm.

## Adding a second device to your account

1. On the phone that is already signed in: **Settings, Add another device**. A code appears, valid
   for 15 minutes, usable once.
2. On the new phone choose **Pair with a code** (not Join) and enter the same server and that code.

An account can have a limited number of devices (10 by default). Sign out of one to free a slot.

## Signing out, moving or leaving

- **Sign out** (Settings) revokes this phone on the server. You can keep or delete the data stored
  on the phone. Signing back in to the same account keeps it; signing in to another account deletes it.
- **Export my data** and **Delete my account** are described above.
- If you lose a phone, the app cannot sign it out remotely: ask the server owner to revoke it
  (`bread-tracker-server revoke`). Its token stops working immediately.

## For the server owner: inviting someone

```sh
docker exec bread-tracker bread-tracker-server invite create      # prints the code, once
docker exec bread-tracker bread-tracker-server user list
```

Send the code and the server address through a private channel. To stop someone without deleting
anything: `user disable NAME`. See [packaging/DEPLOY.md](../packaging/DEPLOY.md) for everything else,
including upgrading the existing single-user deployment.
