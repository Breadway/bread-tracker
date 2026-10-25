import 'dart:async';
import 'dart:io';

import 'package:drift/native.dart';
import 'package:drift/drift.dart' hide isNull;
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
// ignore_for_file: depend_on_referenced_packages
import 'package:flutter_secure_storage_platform_interface/flutter_secure_storage_platform_interface.dart';
import 'package:flutter_secure_storage/test/test_flutter_secure_storage_platform.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:shared_preferences_platform_interface/shared_preferences_platform_interface.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/state.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/data/sync.dart';
import 'package:tracker/fx/sfx.dart';
import 'package:tracker/push.dart';
import 'package:tracker/ui/settings.dart';
import 'package:tracker/ui/pair.dart';
import 'package:tracker/ui/server_settings.dart';

import 'signout_test.dart' as fixture;

class _ReadyPush extends NoPushService {
  @override
  Future<void> register({bool requestPermission = false}) async {
    status.value = PushStatus.ready;
  }
}

class _GatedPreferencesStore extends InMemorySharedPreferencesStore {
  _GatedPreferencesStore(this.entered, this.release)
    : super.withData({'flutter.cursor': 12});
  final Completer<void> entered;
  final Completer<void> release;

  @override
  Future<bool> remove(String key) async {
    if (key == 'flutter.cursor') {
      if (!entered.isCompleted) entered.complete();
      await release.future;
    }
    return super.remove(key);
  }
}

class _CommitGate extends QueryInterceptor {
  bool armed = false;
  bool fail = false;
  final entered = Completer<void>();
  final release = Completer<void>();

  @override
  Future<void> commitTransaction(TransactionExecutor inner) async {
    if (armed) {
      armed = false;
      entered.complete();
      await release.future;
      if (fail) throw StateError('injected transaction commit failure');
    }
    await inner.send();
  }
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  test('provider honors the bootstrap Store instance', () async {
    SharedPreferences.setMockInitialValues({});
    FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({
      'token': 'boot',
    });
    final prefs = await SharedPreferences.getInstance();
    final bootstrapStore = Store(prefs);
    final c = createAppContainer(
      prefs: prefs,
      store: bootstrapStore,
      sfx: AudioSfx(),
      push: NoPushService(),
    );
    addTearDown(c.dispose);
    expect(identical(c.read(storeProvider), bootstrapStore), isTrue);
    expect(await c.read(storeProvider).token, 'boot');
  });

  test('pair claim blocks old signout wipe until replacement persistence completes', () async {
    final entered = Completer<void>();
    final release = Completer<void>();
    final (c, store, db) = await fixture.setup(
      MockClient((_) async {
        entered.complete();
        await release.future;
        return http.Response('{}', 200);
      }),
    );
    addTearDown(db.close);
    final ctl = c.read(controllerProvider);
    final pending = ctl.signOut(wipeLocal: true);
    await entered.future.timeout(const Duration(seconds: 3));
    await ctl.pairedAgain();
    final write = store.saveToken('replacement');
    final serverWrite = store.setServerUrl(
      'https://replacement.example.invalid',
    );
    release.complete();
    await pending.timeout(const Duration(seconds: 3));
    await write;
    await serverWrite;
    expect(await store.token, 'replacement');
    expect(await db.select(db.items).get(), hasLength(1));
    expect(await db.select(db.completions).get(), hasLength(1));
  });

  test(
    'replacement claim waits for actual wipe COMMIT before pairing',
    () async {
      final dir = await Directory.systemTemp.createTemp('track71-commit-');
      addTearDown(() => dir.delete(recursive: true));
      final path = '${dir.path}/tracker.sqlite';
      SharedPreferences.setMockInitialValues({});
      final prefs = await SharedPreferences.getInstance();
      FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({
        'token': 'tok',
      });
      final store = Store(prefs);
      final gate = _CommitGate();
      final db = AppDb(NativeDatabase(File(path)).interceptWith(gate));
      await db.upsertItem(
        ItemsCompanion.insert(
          id: fixture.id1,
          kind: 'chore',
          name: 'retained',
          points: 10,
          updatedAt: 1,
        ),
      );
      await db.setCompletion(fixture.id1, '2026-10-19', true, 2);
      final c = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(prefs),
          storeProvider.overrideWithValue(store),
          dbProvider.overrideWithValue(db),
          apiFactoryProvider.overrideWithValue(
            (base, token) => Api(
              base,
              token: token,
              client: MockClient((_) async => http.Response('{}', 200)),
            ),
          ),
        ],
      );
      addTearDown(c.dispose);
      addTearDown(db.close);
      final ctl = c.read(controllerProvider);
      gate.armed = true;
      final signingOut = ctl.signOut(wipeLocal: true);
      await gate.entered.future.timeout(const Duration(seconds: 3));
      var claimCompleted = false;
      final claim = ctl.pairedAgain().then((_) => claimCompleted = true);
      await Future<void>.delayed(Duration.zero);
      expect(
        claimCompleted,
        isFalse,
        reason: 'pairing cannot claim while destructive commit is unresolved',
      );
      gate.release.complete();
      await signingOut;
      await claim;
      expect(claimCompleted, isTrue);
      await store.saveToken('replacement');
      ctl.pairingPersisted();
      await db.upsertItem(
        ItemsCompanion.insert(
          id: '22222222-2222-4222-8222-222222222222',
          kind: 'todo',
          name: 'replacement row',
          points: 20,
          updatedAt: 3,
        ),
      );
      await db.close();
      final reopened = AppDb(NativeDatabase(File(path)));
      expect(
        (await reopened.select(reopened.items).get()).map((r) => r.name),
        contains('replacement row'),
      );
      await reopened.close();
    },
  );

  test('actual production wipe COMMIT failure rolls back rows and signout fails closed', () async {
    final dir = await Directory.systemTemp.createTemp('track71-commit-fail-');
    addTearDown(() => dir.delete(recursive: true));
    final path = '${dir.path}/tracker.sqlite';
    SharedPreferences.setMockInitialValues({});
    final prefs = await SharedPreferences.getInstance();
    FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({
      'token': 'tok',
    });
    final store = Store(prefs);
    final gate = _CommitGate();
    final db = AppDb(NativeDatabase(File(path)).interceptWith(gate));
    await db.upsertItem(
      ItemsCompanion.insert(
        id: fixture.id1,
        kind: 'chore',
        name: 'preserve on rollback',
        points: 10,
        updatedAt: 1,
      ),
    );
    final c = ProviderContainer(
      overrides: [
        prefsProvider.overrideWithValue(prefs),
        storeProvider.overrideWithValue(store),
        dbProvider.overrideWithValue(db),
        apiFactoryProvider.overrideWithValue(
          (base, token) => Api(
            base,
            token: token,
            client: MockClient((_) async => http.Response('{}', 200)),
          ),
        ),
      ],
    );
    addTearDown(c.dispose);
    gate
      ..armed = true
      ..fail = true;
    final signout = c.read(controllerProvider).signOut(wipeLocal: true);
    await gate.entered.future.timeout(const Duration(seconds: 3));
    gate.release.complete();
    await expectLater(signout, throwsA(isA<StateError>()));
    expect(await store.token, isNull);
    final controller = c.read(controllerProvider);
    expect(await controller.recoverAfterWipeFailure(), isTrue);
    final keepDataRecovery = controller.signOut(wipeLocal: false);
    expect(await keepDataRecovery, isTrue);
    expect(await store.token, isNull);
    await db.close();
    final recovered = AppDb(NativeDatabase(File(path)));
    expect(
      (await recovered.select(recovered.items).get()).single.name,
      'preserve on rollback',
    );
    await recovered.close();
  });

  testWidgets(
    'PairScreen retries local recovery without redeeming one-time code again',
    (tester) async {
      final setup = await tester.runAsync(() async {
        final dir = await Directory.systemTemp.createTemp(
          'track71-pair-recovery-',
        );
        SharedPreferences.setMockInitialValues({});
        final prefs = await SharedPreferences.getInstance();
        FlutterSecureStoragePlatform.instance =
            TestFlutterSecureStoragePlatform({'token': 'tok'});
        final store = Store(prefs);
        final gate = _CommitGate();
        final db = AppDb(
          NativeDatabase(File('${dir.path}/tracker.sqlite'))
              .interceptWith(gate),
        );
        await db.upsertItem(
          ItemsCompanion.insert(
            id: fixture.id1,
            kind: 'chore',
            name: 'keep through recovery',
            points: 10,
            updatedAt: 1,
          ),
        );
        await db.setCompletion(fixture.id1, '2026-10-19', true, 2);
        return (dir, prefs, store, gate, db);
      });
      final (dir, prefs, store, gate, db) = setup!;
      addTearDown(() => dir.delete(recursive: true));
      final pairEntered = Completer<void>();
      var pairCalls = 0;
      final client = MockClient((request) async {
        if (request.url.path == '/pair') {
          pairCalls++;
          pairEntered.complete();
          return http.Response('{"token":"issued-once"}', 200);
        }
        if (request.method == 'DELETE') return http.Response('{}', 200);
        return http.Response('{}', 200);
      });
      final previousClient = Api.sharedClient;
      Api.sharedClient = client;
      addTearDown(() => Api.sharedClient = previousClient);
      final push = _ReadyPush();
      final c = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(prefs),
          storeProvider.overrideWithValue(store),
          dbProvider.overrideWithValue(db),
          apiFactoryProvider.overrideWithValue(
            (base, token) => Api(base, token: token, client: client),
          ),
          pushProvider.overrideWithValue(push),
        ],
      );
      addTearDown(c.dispose);
      addTearDown(db.close);
      await tester.runAsync(() async {
        final controller = c.read(controllerProvider);
        final failedWipe = controller.signOut(wipeLocal: true);
        gate
          ..armed = true
          ..fail = true;
        await gate.entered.future.timeout(const Duration(seconds: 3));
        gate.release.complete();
        await expectLater(failedWipe, throwsA(isA<StateError>()));
      });
      final widget = UncontrolledProviderScope(
        container: c,
        child: const MaterialApp(home: PairScreen()),
      );
      await tester.pumpWidget(widget);
      await tester.pumpAndSettle();
      await tester.enterText(
        find.byType(TextField).at(0),
        'https://tracker.breadway.dev',
      );
      await tester.enterText(find.byType(TextField).at(1), 'ABCD-1234');
      await tester.tap(find.text('Pair'));
      await tester.pump();
      for (var i = 0; i < 40 && !gate.entered.isCompleted; i++) {
        await tester.pump(const Duration(milliseconds: 25));
      }
      expect(
        pairEntered.isCompleted,
        isTrue,
        reason:
            'the PairScreen exchanged the one-time code and cached its token',
      );
      await tester.pumpAndSettle();
      expect(
        find.textContaining('Local recovery is incomplete'),
        findsOneWidget,
      );
      expect(pairCalls, 1);
      await tester.runAsync(() async {
        expect(await store.token, isNull);
      });
      expect(
        await tester.runAsync(() async => db.select(db.items).get()),
        hasLength(1),
      );
      gate.fail = false;
      await tester.tap(find.text('Verify recovery and keep data'));
      await tester.pump();
      for (
        var i = 0;
        i < 40 &&
            find
                .textContaining('Retained data is verified and kept')
                .evaluate()
                .isEmpty;
        i++
      ) {
        await tester.pump(const Duration(milliseconds: 25));
        await tester.runAsync(() async {
          await Future<void>.delayed(const Duration(milliseconds: 10));
        });
      }
      await tester.pumpAndSettle();
      if (find
          .textContaining('Retained data is verified and kept')
          .evaluate()
          .isEmpty) {
        fail(
          'Keep-data recovery did not finish within the bounded UI pump: ${tester.widgetList<Text>(find.byType(Text)).map((t) => t.data).whereType<String>().toList()}',
        );
      }
      expect(
        find.textContaining('Retained data is verified and kept'),
        findsOneWidget,
      );
      await tester.runAsync(() async {
        expect(
          await store.token,
          isNull,
          reason: 'recovery does not claim paired state until the cached token is persisted',
        );
        expect(
          (await db.select(db.items).get()).single.name,
          'keep through recovery',
        );
        expect(await db.select(db.completions).get(), hasLength(1));
      });
      await tester.tap(find.text('Pair'));
      await tester.pump();
      for (
        var i = 0;
        i < 40 &&
            ((await tester.runAsync(() async => store.token)) !=
                    'issued-once' ||
                find
                    .textContaining('Local recovery is incomplete')
                    .evaluate()
                    .isNotEmpty);
        i++
      ) {
        await tester.pump(const Duration(milliseconds: 25));
      }
      await tester.pumpAndSettle();
      expect(
        find.textContaining('Local recovery is incomplete'),
        findsNothing,
        reason: 'the local recovery retry should complete, not remain blocked',
      );
      expect(
        pairCalls,
        1,
        reason: 'the same live screen reused its cached successful exchange',
      );
      await tester.runAsync(() async {
        expect(await store.token, 'issued-once');
        await db.close();
        final finalReopen = AppDb(
          NativeDatabase(File('${dir.path}/tracker.sqlite')),
        );
        expect(
          (await finalReopen.select(finalReopen.items).get()).single.name,
          'keep through recovery',
        );
        expect(
          await finalReopen.select(finalReopen.completions).get(),
          hasLength(1),
        );
        await finalReopen.close();
      });
    },
  );

  test('pairing during DELETE cannot wipe replacement data', () async {
    final enteredDelete = Completer<void>();
    final releaseDelete = Completer<void>();
    final (c, store, db) = await fixture.setup(
      MockClient((r) async {
        if (r.method == 'DELETE') {
          enteredDelete.complete();
          await releaseDelete.future;
        }
        if (r.method == 'POST') {
          return http.Response(
            '{"cursor":13,"items":[],"completions":[]}',
            200,
          );
        }
        return http.Response('{}', 200);
      }),
    );
    addTearDown(db.close);
    addTearDown(() {
      if (!releaseDelete.isCompleted) releaseDelete.complete();
    });
    final ctl = c.read(controllerProvider);
    final signout = ctl.signOut(wipeLocal: true);
    await enteredDelete.future;
    await ctl.pairedAgain();
    final replacementWrite = store.saveToken('replacement');
    final serverWrite = store.setServerUrl(
      'https://replacement.example.invalid',
    );
    expect(await c.read(syncEngineProvider).sync(), SyncStatus.unpaired);
    releaseDelete.complete();
    await signout;
    await replacementWrite;
    await serverWrite;
    expect(await db.select(db.items).get(), hasLength(1));
    expect(await store.token, 'replacement');
    await db.upsertItem(
      ItemsCompanion.insert(
        id: '22222222-2222-4222-8222-222222222222',
        kind: 'todo',
        name: 'replacement data',
        points: 20,
        updatedAt: 9,
      ),
    );
    ctl.pairingPersisted();
    expect(await c.read(syncEngineProvider).sync(), SyncStatus.idle);
    expect(
      (await db.select(db.items).get()).map((r) => r.name),
      contains('replacement data'),
    );
  });

  testWidgets(
    'delayed test-push 401 cannot clear pairing started during request',
    (tester) async {
      final entered = Completer<void>(), release = Completer<void>();
      final (parent, store, db) = await fixture.setup(
        MockClient((r) async => http.Response('{}', 200)),
      );
      addTearDown(db.close);
      addTearDown(() {
        if (!release.isCompleted) release.complete();
      });
      final c = ProviderContainer(
        parent: parent,
        overrides: [
          pushProvider.overrideWithValue(_ReadyPush()),
          apiFactoryProvider.overrideWithValue(
            (b, t) => Api(
              b,
              token: t,
              client: MockClient((r) async {
                if (r.url.path == '/push/test') {
                  if (!entered.isCompleted) entered.complete();
                  await release.future;
                  return http.Response('{}', 401);
                }
                return http.Response('{}', 200);
              }),
            ),
          ),
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
      addTearDown(c.dispose);
      tester.view.physicalSize = const Size(1200, 3000);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: c,
          child: const MaterialApp(home: Scaffold(body: SettingsScreen())),
        ),
      );
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.text('Send a test notification'));
      await tester.tap(find.text('Send a test notification'));
      await tester.pump();
      var requestStarted = false;
      for (var i = 0; i < 20 && !requestStarted; i++) {
        await tester.pump(const Duration(milliseconds: 25));
        requestStarted = entered.isCompleted;
      }
      expect(requestStarted, isTrue, reason: 'test-push HTTP request started');
      final claim = c.read(controllerProvider).pairedAgain();
      release.complete();
      await tester.runAsync(() async {
        await claim;
        final serverWrite = store.setServerUrl('https://new.example.invalid');
        final tokenWrite = store.saveToken('replacement');
        await serverWrite;
        await tokenWrite;
        c.read(controllerProvider).pairingPersisted();
      });
      await tester.pumpAndSettle();
      await tester.pumpAndSettle();
      await tester.runAsync(() async {
        expect(await store.token, 'replacement');
      });
      expect(tester.takeException(), isNull);
    },
  );

  test('signout does not wipe if pairing starts during cursor reset', () async {
    final enteredReset = Completer<void>();
    final releaseReset = Completer<void>();
    SharedPreferences.setMockInitialValues({'cursor': 12});
    final prefs = await SharedPreferences.getInstance();
    final prefsPlatform = _GatedPreferencesStore(enteredReset, releaseReset);
    SharedPreferencesStorePlatform.instance = prefsPlatform;
    FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({
      'token': 'tok',
    });
    final store = Store(prefs);
    final db = AppDb(NativeDatabase.memory());
    await db.upsertItem(
      ItemsCompanion.insert(
        id: fixture.id1,
        kind: 'chore',
        name: 'retained',
        points: 10,
        updatedAt: 1,
      ),
    );
    await db.setCompletion(fixture.id1, '2026-10-19', true, 2);
    final c = ProviderContainer(
      overrides: [
        prefsProvider.overrideWithValue(prefs),
        storeProvider.overrideWithValue(store),
        dbProvider.overrideWithValue(db),
        apiFactoryProvider.overrideWithValue(
          (base, token) => Api(
            base,
            token: token,
            client: MockClient(
              (request) async => request.method == 'POST'
                  ? http.Response(
                      '{"cursor":13,"items":[],"completions":[]}',
                      200,
                    )
                  : http.Response('{}', 200),
            ),
          ),
        ),
      ],
    );
    addTearDown(c.dispose);
    addTearDown(db.close);
    final controller = c.read(controllerProvider);
    final signingOut = controller.signOut(wipeLocal: true);
    await enteredReset.future.timeout(const Duration(seconds: 3));
    await controller.pairedAgain();
    releaseReset.complete();
    await signingOut;
    expect(await db.select(db.items).get(), hasLength(1));
    expect(await db.select(db.completions).get(), hasLength(1));
    await store.saveToken('replacement');
    controller.pairingPersisted();
    expect(await store.token, 'replacement');
  });

  test('failed replacement persistence does not reopen sync admission', () async {
    final (c, store, db) = await fixture.setup(
      MockClient((_) async => http.Response('{}', 200)),
    );
    addTearDown(db.close);
    final ctl = c.read(controllerProvider);
    await ctl.pairedAgain();
    // Simulate failure before the pairing UI's successful persistence callback.
    expect(await c.read(syncEngineProvider).sync(), SyncStatus.unpaired);
    // Without pairingPersisted, the failed pairing path leaves admission closed.
    expect(await c.read(syncEngineProvider).sync(), SyncStatus.unpaired);
    expect(await store.token, 'tok');
    expect(await db.select(db.items).get(), hasLength(1));
  });
  test(
    'delayed signout cannot clear replacement pairing or wipe its data',
    () async {
      final entered = Completer<void>();
      final release = Completer<void>();
      final (c, store, db) = await fixture.setup(
        MockClient((r) async {
          entered.complete();
          await release.future;
          return http.Response('{}', 200);
        }),
      );
      addTearDown(db.close);
      addTearDown(() {
        if (!release.isCompleted) release.complete();
      });
      final ctl = c.read(controllerProvider);
      final pending = ctl.signOut(wipeLocal: true);
      await entered.future;
      await store.saveToken('replacement');
      await ctl.pairedAgain();
      release.complete();
      await pending;
      expect(
        await store.token,
        'replacement',
        reason: 'old signout owns old credential only',
      );
      expect(
        await db.select(db.items).get(),
        hasLength(1),
        reason: 'new account data cannot be wiped by old operation',
      );
    },
  );

  test('delayed signout cannot wipe replacement pairing data', () async {
    final entered = Completer<void>();
    final release = Completer<void>();
    final (c, store, db) = await fixture.setup(
      MockClient((r) async {
        entered.complete();
        await release.future;
        return http.Response('{}', 200);
      }),
    );
    addTearDown(db.close);
    addTearDown(() {
      if (!release.isCompleted) release.complete();
    });
    final ctl = c.read(controllerProvider);
    final pending = ctl.signOut(wipeLocal: true);
    await entered.future;
    await store.saveToken('replacement');
    await ctl.pairedAgain();
    release.complete();
    await pending;
    expect(
      await db.select(db.items).get(),
      hasLength(1),
      reason: 'new account data cannot be wiped by old operation',
    );
  });

  test(
    'old server without DELETE still signs out locally without data loss',
    () async {
      final (c, store, db) = await fixture.setup(
        MockClient((r) async => http.Response('<html>404</html>', 404)),
      );
      addTearDown(db.close);
      expect(await c.read(controllerProvider).signOut(), false);
      expect(await store.token, isNull);
      expect(await db.select(db.items).get(), hasLength(1));
      expect(await db.select(db.completions).get(), hasLength(1));
    },
  );
}
