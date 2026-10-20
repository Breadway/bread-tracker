import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'app.dart';
import 'data/store.dart';
import 'fx/sfx.dart';
import 'push.dart';
import 'state.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final prefs = await SharedPreferences.getInstance();
  final store = Store(prefs);
  final sfx = AudioSfx()
    ..sound = store.sound
    ..haptics = store.haptics;
  await sfx.init();
  final push = FcmPushService(store);
  runApp(
    ProviderScope(
      overrides: [
        prefsProvider.overrideWithValue(prefs),
        sfxProvider.overrideWithValue(sfx),
        pushProvider.overrideWithValue(push),
      ],
      child: const TrackerApp(),
    ),
  );
  // Never hold the first frame behind Firebase or a network request.
  push.init();
}
