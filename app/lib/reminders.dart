import 'dart:async';
import 'dart:convert';
import 'dart:ui' show DartPluginRegistrant;

import 'package:drift/drift.dart' show BooleanExpressionOperators;
import 'package:firebase_core/firebase_core.dart';
import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_local_notifications/flutter_local_notifications.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'data/database.dart';
import 'data/store.dart';
import 'data/sync.dart';

/// Android notification channel used for reminders (also created by the push service).
const reminderChannelId = 'reminders';

/// Id of the "Mark done" notification action.
const markDoneActionId = 'mark_done';

/// What a reminder is about, carried in the FCM data and in the local notification payload.
class ReminderPayload {
  const ReminderPayload({
    required this.itemId,
    required this.date,
    required this.kind,
    this.title,
    this.body,
  });

  final String itemId, date, kind;
  final String? title, body;

  /// Parse FCM `data` (all values are strings). Returns null when it is not a reminder.
  static ReminderPayload? fromData(Map<String, dynamic> data) {
    final id = data['item_id'], date = data['date'], kind = data['kind'];
    if (id is! String || id.isEmpty || date is! String || date.isEmpty) {
      return null;
    }
    if (kind is! String || kind == 'test') return null;
    return ReminderPayload(
      itemId: id,
      date: date,
      kind: kind,
      title: data['title'] as String?,
      body: data['body'] as String?,
    );
  }

  /// The string stored on the local notification and handed back on tap or action.
  String encode() =>
      jsonEncode({'item_id': itemId, 'date': date, 'kind': kind});

  static ReminderPayload? decode(String? s) {
    if (s == null || s.isEmpty) return null;
    try {
      final j = jsonDecode(s);
      return j is Map<String, dynamic> ? fromData(j) : null;
    } on FormatException {
      return null;
    }
  }

  /// A stable notification id, so a repeat for the same item and day replaces itself.
  int get notificationId => Object.hash(itemId, date) & 0x7fffffff;
}

/// Details for a reminder notification, with the Mark done action.
NotificationDetails reminderNotificationDetails() => const NotificationDetails(
  android: AndroidNotificationDetails(
    reminderChannelId,
    'Reminders',
    channelDescription: 'Chore, habit and to-do reminders',
    importance: Importance.high,
    priority: Priority.high,
    icon: '@drawable/ic_stat_bread',
    actions: [
      AndroidNotificationAction(
        markDoneActionId,
        'Mark done',
        // Handled by a background isolate; the app does not open.
        showsUserInterface: false,
        cancelNotification: true,
      ),
    ],
  ),
);

/// Record a completion for the reminded item, exactly as ticking it in the app would:
/// a dirty local row that the next sync pushes. Returns whether anything was written.
///
/// Nothing is written when the item is gone, is not a scheduled kind, or is already done
/// (a to-do counts as done if it was completed on any day).
Future<bool> markReminderDone(
  AppDb db,
  ReminderPayload p, {
  int Function()? nowMs,
}) async {
  final item = await (db.select(db.items)..where((t) => t.id.equals(p.itemId)))
      .getSingleOrNull();
  if (item == null || item.deletedAt != null) return false;
  final done = await (db.select(db.completions)
        ..where((t) {
          final live = t.itemId.equals(p.itemId) & t.deletedAt.isNull();
          // A to-do is done once, on whichever day it was ticked; chores and habits per day.
          return item.kind == 'todo' ? live : live & t.date.equals(p.date);
        }))
      .get();
  if (done.isNotEmpty) return false;
  await db.setCompletion(
    p.itemId,
    p.date,
    true,
    (nowMs ?? () => DateTime.now().millisecondsSinceEpoch)(),
  );
  return true;
}

/// Handle a tapped notification action. Opens nothing; only writes the completion and, if
/// [sync] is given, tries to push it (failures are fine: the row stays dirty and the app
/// syncs it the next time it runs).
Future<bool> handleReminderAction(
  String? actionId,
  String? payload, {
  required AppDb db,
  Future<void> Function()? sync,
  int Function()? nowMs,
}) async {
  if (actionId != markDoneActionId) return false;
  final p = ReminderPayload.decode(payload);
  if (p == null) return false;
  final wrote = await markReminderDone(db, p, nowMs: nowMs);
  if (sync != null) {
    try {
      await sync();
    } catch (_) {}
  }
  return wrote;
}

/// Show a reminder as a local notification with the Mark done action.
Future<void> showReminderNotification(
  FlutterLocalNotificationsPlugin plugin,
  ReminderPayload p,
) => plugin.show(
  id: p.notificationId,
  title: p.title,
  body: p.body,
  notificationDetails: reminderNotificationDetails(),
  payload: p.encode(),
);

Future<void> initLocalNotifications(
  FlutterLocalNotificationsPlugin plugin, {
  DidReceiveNotificationResponseCallback? onResponse,
}) => plugin.initialize(
  settings: const InitializationSettings(
    android: AndroidInitializationSettings('@drawable/ic_stat_bread'),
  ),
  onDidReceiveNotificationResponse: onResponse,
  onDidReceiveBackgroundNotificationResponse: reminderActionInBackground,
);

/// Runs in a background isolate when the user taps "Mark done" on a notification.
@pragma('vm:entry-point')
Future<void> reminderActionInBackground(NotificationResponse r) async {
  DartPluginRegistrant.ensureInitialized();
  final db = AppDb();
  try {
    await handleReminderAction(
      r.actionId,
      r.payload,
      db: db,
      sync: () async {
        final prefs = await SharedPreferences.getInstance();
        await SyncEngine(db, Store(prefs)).sync().timeout(
          const Duration(seconds: 25),
        );
      },
    );
  } catch (e) {
    debugPrint('Mark done from notification failed: $e');
  } finally {
    await db.close();
  }
}

/// Runs in a background isolate for data-only reminder messages (server push mode `data`)
/// when the app is not in the foreground. Builds the notification itself so it can carry
/// the Mark done action. Anything that is not a reminder is ignored.
@pragma('vm:entry-point')
Future<void> reminderBackgroundMessage(RemoteMessage message) async {
  DartPluginRegistrant.ensureInitialized();
  final p = ReminderPayload.fromData(message.data);
  // A message with a notification block is shown by Android itself.
  if (p == null || message.notification != null) return;
  final plugin = FlutterLocalNotificationsPlugin();
  try {
    await Firebase.initializeApp();
    await initLocalNotifications(plugin);
    await showReminderNotification(plugin, p);
  } catch (e) {
    debugPrint('Could not show background reminder: $e');
  }
}
