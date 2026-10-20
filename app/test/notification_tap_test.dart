import 'dart:async';

import 'package:drift/drift.dart' show driftRuntimeOptions;
import 'package:drift/native.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/app.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/push.dart';
import 'package:tracker/reminders.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/pair.dart';

class NoTokenStore extends Store {
  NoTokenStore(super.prefs);
  @override
  Future<String?> get token async => null;
}

class TapPush extends NoPushService {
  final controller = StreamController<ReminderPayload>.broadcast();
  @override
  Stream<ReminderPayload> get taps => controller.stream;
}

void main() {
  driftRuntimeOptions.dontWarnAboutMultipleDatabases = true;

  testWidgets('tapping a reminder opens Today on the reminded day', (tester) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await SharedPreferences.getInstance();
    final push = TapPush();
    final container = ProviderContainer(overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(NoTokenStore(prefs)),
      dbProvider.overrideWithValue(AppDb(NativeDatabase.memory())),
      pairedProvider.overrideWith((ref) async => true),
      pushProvider.overrideWithValue(push),
    ]);
    addTearDown(container.dispose);
    tester.view.physicalSize = const Size(1080, 2340);
    tester.view.devicePixelRatio = 2.625;
    addTearDown(tester.view.reset);
    await tester.pumpWidget(UncontrolledProviderScope(container: container, child: const TrackerApp()));
    await tester.pump(const Duration(milliseconds: 600));

    // Go to Settings, then a reminder for an earlier day is tapped.
    await tester.tap(find.text('Settings').last);
    await tester.pump(const Duration(milliseconds: 300));
    expect(tester.widget<NavigationBar>(find.byType(NavigationBar)).selectedIndex, 3);

    push.controller.add(const ReminderPayload(itemId: 'i', date: '2026-10-12', kind: 'chore'));
    await tester.pump(const Duration(milliseconds: 100));
    expect(tester.widget<NavigationBar>(find.byType(NavigationBar)).selectedIndex, 0, reason: 'Today tab');
    expect(container.read(selectedDayProvider), DateTime.utc(2026, 10, 12));

    // A later tap for the same day still navigates again.
    await tester.tap(find.text('Settings').last);
    await tester.pump(const Duration(milliseconds: 300));
    push.controller.add(const ReminderPayload(itemId: 'i', date: '2026-10-12', kind: 'chore'));
    await tester.pump(const Duration(milliseconds: 100));
    expect(tester.widget<NavigationBar>(find.byType(NavigationBar)).selectedIndex, 0);
    await tester.pump(const Duration(seconds: 1));
  });
}
