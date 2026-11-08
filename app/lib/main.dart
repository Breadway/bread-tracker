import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'app.dart';
import 'data/store.dart';
import 'fx/sfx.dart';
import 'push.dart';
import 'reminders.dart';
import 'state.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final prefs = await SharedPreferences.getInstance();
  final store = Store(prefs);
  final sfx = AudioSfx()
    ..sound = store.sound
    ..haptics = store.haptics;
  await sfx.init();
  // Data-only reminders (server BREADTRACKER_PUSH_MODE=data) are turned into notifications
  // with a Mark done action by this top-level handler, in a background isolate.
  FirebaseMessaging.onBackgroundMessage(reminderBackgroundMessage);
  late final ProviderContainer container;
  final push = FcmPushService(
    store,
    onAuthLost: () =>
        container.read(pairedStateRevisionProvider.notifier).refresh(),
  );
  container = createAppContainer(
    prefs: prefs,
    store: store,
    sfx: sfx,
    push: push,
  );
  runApp(
    UncontrolledProviderScope(container: container, child: const TrackerApp()),
  );
  // Never hold the first frame behind Firebase or a network request.
  push.init();
}
