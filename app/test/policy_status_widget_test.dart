import 'package:drift/native.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/sync.dart';
import 'package:tracker/fx/sfx.dart';
import 'package:tracker/push.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/pair.dart';
import 'package:tracker/ui/server_settings.dart';
import 'package:tracker/ui/settings.dart';

import 'sync_test.dart' show MemStore;

class StubSyncEngine extends SyncEngine {
  StubSyncEngine(super.db, super.store);
  int calls = 0;

  @override
  Future<SyncStatus> sync() async {
    calls++;
    return SyncStatus.idle;
  }
}

Future<(ProviderContainer, StubSyncEngine)> pumpPolicy(
  WidgetTester tester,
  SyncStatus status,
  String message,
) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final store = MemStore(prefs);
  final db = AppDb(NativeDatabase.memory());
  addTearDown(db.close);
  final engine = StubSyncEngine(db, store)..rejectionMessage = message;
  final container = ProviderContainer(
    overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(store),
      syncEngineProvider.overrideWithValue(engine),
      pairedProvider.overrideWith((ref) async => true),
      sfxProvider.overrideWithValue(NoSfx()),
      pushProvider.overrideWithValue(NoPushService()),
      serverSettingsProvider.overrideWith(
        (ref) async => const ServerSettings(
          timezone: 'UTC',
          quietStart: '',
          quietEnd: '',
          remindersEnabled: true,
          utcOffsetMinutes: 0,
        ),
      ),
    ],
  );
  addTearDown(container.dispose);
  container.read(syncStatusProvider.notifier).set(status);
  tester.view.physicalSize = const Size(1200, 3200);
  tester.view.devicePixelRatio = 1;
  addTearDown(tester.view.reset);
  await tester.pumpWidget(
    UncontrolledProviderScope(
      container: container,
      child: const MaterialApp(home: Scaffold(body: SettingsScreen())),
    ),
  );
  await tester.pumpAndSettle();
  return (container, engine);
}

void main() {
  testWidgets('quota refusal shows an actionable recovery and retry', (
    tester,
  ) async {
    final (container, engine) = await pumpPolicy(
      tester,
      SyncStatus.quotaExhausted,
      'item limit reached (2000 per account)',
    );
    expect(find.textContaining('reached a server limit'), findsOneWidget);
    expect(find.textContaining('2000 per account'), findsOneWidget);
    expect(find.text('Retry sync'), findsOneWidget);

    await tester.tap(find.text('Retry sync'));
    await tester.pumpAndSettle();
    expect(engine.calls, 1);
    expect(find.text('Up to date'), findsOneWidget);
    expect(find.text('Sync retried'), findsOneWidget);
    expect(engine.rejectionMessage, 'item limit reached (2000 per account)');
    container.read(syncStatusProvider.notifier).set(SyncStatus.quotaExhausted);
    await tester.pump();
    expect(find.textContaining('reached a server limit'), findsOneWidget);
  });

  testWidgets('disabled account tells the user to ask the server owner', (
    tester,
  ) async {
    await pumpPolicy(
      tester,
      SyncStatus.accountDisabled,
      'this account is disabled',
    );
    expect(
      find.textContaining('this account is disabled on the server'),
      findsOneWidget,
    );
    expect(find.textContaining('Ask the server owner'), findsOneWidget);
    expect(find.text('Retry sync'), findsOneWidget);
  });

  testWidgets('rate limiting tells the user to wait and retry', (tester) async {
    await pumpPolicy(
      tester,
      SyncStatus.rateLimited,
      'too many attempts, try again later',
    );
    expect(find.textContaining('rate limiting this device'), findsOneWidget);
    expect(find.textContaining('Wait a minute'), findsOneWidget);
    expect(find.text('Retry sync'), findsOneWidget);
  });
}
