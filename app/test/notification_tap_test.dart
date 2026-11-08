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

class FirebaseTapPush extends FcmPushService {
  FirebaseTapPush(super.store, {required super.device});
  @override
  Future<void> register({bool requestPermission = false}) async {}
}

void main() {
  driftRuntimeOptions.dontWarnAboutMultipleDatabases = true;

  testWidgets('tapping a reminder opens Today on the reminded day', (
    tester,
  ) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await SharedPreferences.getInstance();
    final push = TapPush();
    final container = ProviderContainer(
      overrides: [
        prefsProvider.overrideWithValue(prefs),
        storeProvider.overrideWithValue(NoTokenStore(prefs)),
        dbProvider.overrideWithValue(AppDb(NativeDatabase.memory())),
        pairedProvider.overrideWith((ref) async => true),
        pushProvider.overrideWithValue(push),
      ],
    );
    addTearDown(container.dispose);
    tester.view.physicalSize = const Size(1080, 2340);
    tester.view.devicePixelRatio = 2.625;
    addTearDown(tester.view.reset);
    await tester.pumpWidget(
      UncontrolledProviderScope(
        container: container,
        child: const TrackerApp(),
      ),
    );
    await tester.pump(const Duration(milliseconds: 600));

    // Go to Settings, then a reminder for an earlier day is tapped.
    await tester.tap(find.text('Settings').last);
    await tester.pump(const Duration(milliseconds: 300));
    expect(
      tester.widget<NavigationBar>(find.byType(NavigationBar)).selectedIndex,
      3,
    );

    push.controller.add(
      const ReminderPayload(itemId: 'i', date: '2026-10-12', kind: 'chore'),
    );
    await tester.pump(const Duration(milliseconds: 100));
    expect(
      tester.widget<NavigationBar>(find.byType(NavigationBar)).selectedIndex,
      0,
      reason: 'Today tab',
    );
    expect(container.read(selectedDayProvider), DateTime.utc(2026, 10, 12));

    // A later tap for the same day still navigates again.
    await tester.tap(find.text('Settings').last);
    await tester.pump(const Duration(milliseconds: 300));
    push.controller.add(
      const ReminderPayload(itemId: 'i', date: '2026-10-12', kind: 'chore'),
    );
    await tester.pump(const Duration(milliseconds: 100));
    expect(
      tester.widget<NavigationBar>(find.byType(NavigationBar)).selectedIndex,
      0,
    );
    await tester.pump(const Duration(seconds: 1));
  });

  test(
    'production push device replays and acknowledges a delayed cold-start tap',
    () async {
      final gate = Completer<ReminderPayload?>();
      final taps = <ReminderPayload>[];
      var acknowledged = false;
      final d = FirebasePushDevice();
      d.taps.listen((payload) {
        taps.add(payload);
        d.acknowledgeTap(payload);
        acknowledged = true;
      });
      final capture = d.captureInitialTapForTest(() => gate.future);
      gate.complete(
        const ReminderPayload(
          itemId: 'cold',
          date: '2026-10-12',
          kind: 'chore',
        ),
      );
      await capture;
      d.replayPendingTap();
      await Future<void>.delayed(Duration.zero);
      expect(taps, [
        const ReminderPayload(
          itemId: 'cold',
          date: '2026-10-12',
          kind: 'chore',
        ),
      ]);
      expect(acknowledged, isTrue);
      d.dispose();
    },
  );

  testWidgets(
    'Gate delays Shell until production launch tap is buffered, then routes',
    (tester) async {
      final gate = Completer<void>();
      final device = FirebasePushDevice();
      SharedPreferences.setMockInitialValues({});
      final prefs = await SharedPreferences.getInstance();
      final service = FirebaseTapPush(NoTokenStore(prefs), device: device);
      final db = AppDb(NativeDatabase.memory());
      final container = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(prefs),
          storeProvider.overrideWithValue(NoTokenStore(prefs)),
          dbProvider.overrideWithValue(db),
          pairedProvider.overrideWith((ref) => gate.future.then((_) => true)),
          pushProvider.overrideWithValue(service),
        ],
      );
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: container,
          child: const MaterialApp(home: Gate()),
        ),
      );
      await tester.pump();
      expect(find.byType(Shell), findsNothing, reason: 'Gate is still loading');
      final launchCapture = device.captureInitialTapForTest(() async {
        await gate.future;
        return const ReminderPayload(
          itemId: 'i',
          date: '2026-10-12',
          kind: 'chore',
        );
      });
      gate.complete();
      await tester.pump();
      await launchCapture;
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 100));
      expect(container.read(selectedDayProvider), DateTime.utc(2026, 10, 12));
      addTearDown(() async {
        await tester.pumpWidget(const SizedBox.shrink());
        container.dispose();
        await db.close();
        device.dispose();
      });
    },
  );
}
