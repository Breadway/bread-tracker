import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'app.dart';
import 'data/store.dart';
import 'fx/sfx.dart';
import 'push.dart';
import 'state.dart';
import 'ui/pair.dart' show pairedProvider;

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final prefs = await SharedPreferences.getInstance();
  final store = Store(prefs);
  final sfx = AudioSfx()
    ..sound = store.sound
    ..haptics = store.haptics;
  await sfx.init();
  late final ProviderContainer container;
  final push = FcmPushService(
    store,
    onAuthLost: () => container.invalidate(pairedProvider),
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
