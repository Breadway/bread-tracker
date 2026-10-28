import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
// ignore_for_file: depend_on_referenced_packages
import 'package:flutter_secure_storage_platform_interface/flutter_secure_storage_platform_interface.dart';
import 'package:flutter_secure_storage/test/test_flutter_secure_storage_platform.dart';
import 'package:shared_preferences_platform_interface/shared_preferences_platform_interface.dart';
import 'package:drift/native.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/data/sync.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/pair.dart';
import 'package:tracker/fx/sfx.dart';
import 'package:tracker/push.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/ui/settings.dart';

class FailDeleteOnce extends TestFlutterSecureStoragePlatform {
  FailDeleteOnce({required this.noOp}) : super({'token': 'existing-token'});
  final bool noOp;
  bool failed = false;

  @override
  Future<void> delete({
    required String key,
    required Map<String, String> options,
  }) async {
    if (!failed) {
      failed = true;
      if (noOp) return;
      throw StateError('injected credential deletion failure');
    }
    await super.delete(key: key, options: options);
  }
}

class FailWrite extends TestFlutterSecureStoragePlatform {
  FailWrite() : super({'token': 'existing-token'});
  bool armed = true;
  @override
  Future<void> write({
    required String key,
    required String value,
    required Map<String, String> options,
  }) async {
    if (armed) {
      armed = false;
      throw StateError('injected secure write failure before mutation');
    }
    await super.write(key: key, value: value, options: options);
  }
}

class FailWriteAfterMutation extends TestFlutterSecureStoragePlatform {
  FailWriteAfterMutation() : super({'token': 'old-token'});
  var armed = true;

  @override
  Future<void> write({
    required String key,
    required String value,
    required Map<String, String> options,
  }) async {
    if (!armed) {
      await super.write(key: key, value: value, options: options);
      return;
    }
    armed = false;
    await super.write(key: key, value: value, options: options);
    throw StateError('injected secure write failure after mutation');
  }
}

class FailSecureRollback extends TestFlutterSecureStoragePlatform {
  FailSecureRollback() : super({});
  var writes = 0;

  @override
  Future<void> write({
    required String key,
    required String value,
    required Map<String, String> options,
  }) async {
    writes++;
    if (writes == 1) {
      await super.write(key: key, value: value, options: options);
      return;
    }
    throw StateError('injected credential rollback failure');
  }

  @override
  Future<void> delete({
    required String key,
    required Map<String, String> options,
  }) async {
    throw StateError('injected credential delete rollback failure');
  }
}

class FailServerSave extends InMemorySharedPreferencesStore {
  FailServerSave()
    : super.withData({
        'flutter.server': 'https://old.invalid',
        'flutter.cursor': 12,
      });
  @override
  Future<bool> setValue(String type, String key, Object value) async {
    if (key == 'flutter.server') return false;
    return super.setValue(type, key, value);
  }
}

class FailPreferenceAfterMutation extends InMemorySharedPreferencesStore {
  FailPreferenceAfterMutation(this.operation)
    : super.withData({
        'flutter.server': 'https://old.invalid',
        'flutter.cursor': 12,
      });
  final String operation;
  var failed = false;

  @override
  Future<bool> setValue(String type, String key, Object value) async {
    final matches =
        (operation == 'server' && key == 'flutter.server') ||
        (operation == 'cursor' && key == 'flutter.cursor');
    if (!failed && matches) {
      failed = true;
      await super.setValue(type, key, value);
      return false;
    }
    return super.setValue(type, key, value);
  }

  @override
  Future<bool> remove(String key) async {
    if (!failed && operation == 'remove' && key == 'flutter.cursor') {
      failed = true;
      await super.remove(key);
      return false;
    }
    return super.remove(key);
  }
}

class FailPreferenceOperation extends InMemorySharedPreferencesStore {
  FailPreferenceOperation(this.operation, {this.throws = false})
    : super.withData({
        'flutter.server': 'https://old.invalid',
        'flutter.cursor': 12,
      });
  final String operation;
  final bool throws;
  var failed = false;

  @override
  Future<bool> setValue(String type, String key, Object value) async {
    if (!failed &&
        ((operation == 'server' && key == 'flutter.server') ||
            (operation == 'cursor' && key == 'flutter.cursor'))) {
      failed = true;
      if (throws) throw StateError('injected $operation persistence exception');
      return false;
    }
    return super.setValue(type, key, value);
  }

  @override
  Future<bool> remove(String key) async {
    if (!failed && operation == 'remove' && key == 'flutter.cursor') {
      failed = true;
      if (throws) throw StateError('injected cursor remove exception');
      return false;
    }
    return super.remove(key);
  }
}

class _ReadyPushService extends NoPushService {
  @override
  Future<void> register({bool requestPermission = false}) async {
    status.value = PushStatus.ready;
  }
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  test(
    'existing install preserves paired state at cold provider startup',
    () async {
      SharedPreferences.setMockInitialValues({
        'server': 'https://old.invalid',
        'cursor': 12,
      });
      FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({
        'token': 'existing-token',
      });
      final prefs = await SharedPreferences.getInstance();
      final store = Store(prefs);
      final c = createAppContainer(
        prefs: prefs,
        store: store,
        sfx: AudioSfx(),
        push: NoPushService(),
      );
      addTearDown(c.dispose);
      expect(
        await c.read(pairedProvider.future),
        isTrue,
        reason: 'existing token must not need a fresh PairScreen exchange',
      );
    },
  );

  for (final choice in [
    'keep',
    'wipe',
    'cancel',
    'delete-throw',
    'delete-noop',
    'delete-stale-retry',
  ]) {
    testWidgets('real Settings sign-out confirmation: $choice', (tester) async {
      SharedPreferences.setMockInitialValues({'server': 'https://old.invalid'});
      final deletionFailure = choice.startsWith('delete-');
      FlutterSecureStoragePlatform.instance = deletionFailure
          ? FailDeleteOnce(noOp: choice != 'delete-throw')
          : TestFlutterSecureStoragePlatform({'token': 'existing-token'});
      final prefs = await SharedPreferences.getInstance();
      final store = Store(prefs);
      final db = AppDb(NativeDatabase.memory());
      final push = _ReadyPushService();
      await db.upsertItem(
        ItemsCompanion.insert(
          id: 'retained',
          kind: 'chore',
          name: 'Retained item',
          points: 1,
          updatedAt: 1,
        ),
      );
      final requests = <String>[];
      final client = MockClient((request) async {
        requests.add('${request.method} ${request.url.path}');
        return http.Response('{}', 200);
      });
      final oldClient = Api.sharedClient;
      Api.sharedClient = client;
      addTearDown(() => Api.sharedClient = oldClient);
      final container = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(prefs),
          storeProvider.overrideWithValue(store),
          dbProvider.overrideWithValue(db),
          apiFactoryProvider.overrideWithValue(
            (base, token) => Api(base, token: token, client: client),
          ),
          sfxProvider.overrideWithValue(NoSfx()),
          pushProvider.overrideWithValue(push),
        ],
      );
      addTearDown(container.dispose);
      addTearDown(db.close);
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: container,
          child: const MaterialApp(home: Scaffold(body: SettingsScreen())),
        ),
      );
      container.read(syncStatusProvider.notifier).set(SyncStatus.idle);
      await tester.pumpAndSettle();
      expect(find.text('Sign out'), findsOneWidget);
      await tester.tap(find.text('Sign out'));
      await tester.pumpAndSettle();
      expect(find.text('Sign out of this phone?'), findsOneWidget);
      final confirmLabel = switch (choice) {
        'keep' => 'Sign out',
        'wipe' => 'Sign out and delete data',
        'delete-throw' ||
        'delete-noop' ||
        'delete-stale-retry' => 'Sign out and delete data',
        _ => 'Cancel',
      };
      await tester.tap(find.text(confirmLabel).last);
      await tester.pump();
      await tester.runAsync(() async {
        await Future<void>.delayed(const Duration(milliseconds: 100));
      });
      await tester.pumpAndSettle();
      expect(tester.takeException(), isNull);
      if (deletionFailure) {
        expect(await store.token, 'existing-token');
        expect(await db.select(db.items).get(), hasLength(1));
        expect(find.textContaining('Sign-out cleanup failed'), findsOneWidget);
        expect(
          await container.read(syncEngineProvider).sync(),
          SyncStatus.unpaired,
        );
        if (choice == 'delete-stale-retry') {
          final controller = container.read(controllerProvider);
          await controller.pairedAgain();
          final owner = store.pairingOwnership('https://replacement.invalid');
          await store.persistPairing(
            owner,
            'https://replacement.invalid',
            'new-owner-token',
          );
          controller.pairingPersisted();
          container.read(pairedStateRevisionProvider.notifier).refresh();
          expect(await container.read(pairedProvider.future), isTrue);
          await tester.pumpAndSettle();
          final requestsBeforeRetry = requests.length;
          await tester.tap(find.text('Retry'));
          await tester.pumpAndSettle();
          expect(tester.takeException(), isNull);
          expect(requests.length, requestsBeforeRetry);
          expect(await store.token, 'new-owner-token');
          expect(await container.read(pairedProvider.future), isTrue);
          expect(await db.select(db.items).get(), hasLength(1));
          return;
        }
        await tester.tap(find.text('Retry'));
        await tester.pump();
        await tester.runAsync(() async {
          await Future<void>.delayed(const Duration(milliseconds: 100));
        });
        await tester.pumpAndSettle();
        expect(tester.takeException(), isNull);
        expect(await store.token, isNull);
        expect(await container.read(pairedProvider.future), isFalse);
        expect(await db.select(db.items).get(), isEmpty);
        return;
      }
      if (choice == 'cancel') {
        expect(requests, isNot(contains('DELETE /device')));
        expect(await store.token, 'existing-token');
        expect(await container.read(pairedProvider.future), isTrue);
        expect(find.text('Sign out'), findsOneWidget);
        expect(await db.select(db.items).get(), hasLength(1));
      } else {
        expect(requests, contains('DELETE /device'));
        expect(await store.token, isNull);
        expect(await container.read(pairedProvider.future), isFalse);
        expect(find.text('Sign out'), findsNothing);
        expect(find.text('Join or pair'), findsOneWidget);
        expect(
          await db.select(db.items).get(),
          hasLength(choice == 'keep' ? 1 : 0),
        );
      }
    });
  }

  test(
    'secure write failure restores old server token cursor and retained fileDB',
    () async {
      SharedPreferences.setMockInitialValues({
        'server': 'https://old.invalid',
        'cursor': 12,
      });
      final secure = FailWrite();
      FlutterSecureStoragePlatform.instance = secure;
      final prefs = await SharedPreferences.getInstance();
      final store = Store(prefs);
      final dir = await Directory('/tmp/opencode/review-pr36-31d7')
          .createTemp('secure-fail-');
      final file = File('${dir.path}/db.sqlite');
      final db = AppDb(NativeDatabase(file));
      await db.upsertItem(
        ItemsCompanion.insert(
          id: 'x',
          kind: 'chore',
          name: 'retained',
          points: 10,
          updatedAt: 1,
        ),
      );
      await db.setCompletion('x', '2026-10-25', true, 2);
      store.claimPairingOwnership();
      final owner = store.pairingOwnership('https://new.invalid');
      await expectLater(
        store.persistPairing(owner, 'https://new.invalid', 'replacement'),
        throwsA(isA<PairingPersistenceFailure>()),
      );
      expect(await store.token, 'existing-token');
      expect(store.cursor, 12);
      await db.close();
      final reopen = AppDb(NativeDatabase(file));
      expect(await reopen.select(reopen.items).get(), hasLength(1));
      expect(await reopen.select(reopen.completions).get(), hasLength(1));
      await reopen.close();
      expect(
        store.serverUrl,
        'https://old.invalid',
        reason: 'failed secure write cannot associate old credential with attempted new server',
      );
    },
  );
  test('durable preference failure is not reported as successful pairing persistence', () async {
    SharedPreferences.setMockInitialValues({
      'server': 'https://old.invalid',
      'cursor': 12,
    });
    SharedPreferencesStorePlatform.instance = FailServerSave();
    FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform(
      {},
    );
    final prefs = await SharedPreferences.getInstance();
    final store = Store(prefs);
    store.claimPairingOwnership();
    final owner = store.pairingOwnership('https://new.invalid');
    await expectLater(
      store.persistPairing(owner, 'https://new.invalid', 'replacement'),
      throwsA(anything),
      reason: 'platform false means server URL was not durably persisted',
    );
    expect(store.serverUrl, 'https://old.invalid');
    expect(await store.token, isNull);
  });
  for (final op in ['server', 'cursor', 'remove']) {
    for (final throws in [false, true]) {
      test(
        'preference $op ${throws ? 'throw' : 'false'} fails closed and reloads persisted state',
        () async {
          final platform = FailPreferenceOperation(op, throws: throws);
          SharedPreferences.setMockInitialValues({
            'server': 'https://old.invalid',
            'cursor': 12,
          });
          SharedPreferencesStorePlatform.instance = platform;
          FlutterSecureStoragePlatform.instance =
              TestFlutterSecureStoragePlatform({});
          final prefs = await SharedPreferences.getInstance();
          final store = Store(prefs);
          if (op == 'cursor') {
            await expectLater(store.setCursor(0), throwsA(anything));
          } else {
            store.claimPairingOwnership();
            final owner = store.pairingOwnership('https://old.invalid');
            await expectLater(
              store.persistPairing(owner, 'https://new.invalid', 'replacement'),
              throwsA(anything),
            );
          }
          expect(
            store.serverUrl,
            'https://old.invalid',
            reason: 'false result must reload the platform snapshot, not trust mutated preferences cache',
          );
          expect(
            store.cursor,
            12,
            reason:
                'attempt rollback must restore cursor on an old-token failure',
          );
          if (op != 'cursor') expect(await store.token, isNull);
        },
      );
    }
  }

  for (final operation in ['server', 'cursor', 'remove']) {
    test(
      '$operation preference false-after-mutation reloads and compensates',
      () async {
        SharedPreferences.setMockInitialValues({
          'server': 'https://old.invalid',
          'cursor': 12,
        });
        final platform = FailPreferenceAfterMutation(operation);
        SharedPreferencesStorePlatform.instance = platform;
        FlutterSecureStoragePlatform.instance =
            TestFlutterSecureStoragePlatform({});
        final prefs = await SharedPreferences.getInstance();
        final store = Store(prefs);
        if (operation == 'cursor') {
          await expectLater(store.setCursor(0), throwsStateError);
        } else {
          store.claimPairingOwnership();
          final owner = store.pairingOwnership('https://old.invalid');
          await expectLater(
            store.persistPairing(owner, 'https://new.invalid', 'issued-token'),
            throwsA(anything),
          );
        }
        await prefs.reload();
        expect(store.serverUrl, 'https://old.invalid');
        expect(store.cursor, 12);
        expect(await store.token, isNull);
      },
    );
  }

  test(
    'secure write failure after token mutation restores all prior components',
    () async {
      SharedPreferences.setMockInitialValues({
        'server': 'https://old.invalid',
        'cursor': 12,
      });
      FlutterSecureStoragePlatform.instance = FailWriteAfterMutation();
      final prefs = await SharedPreferences.getInstance();
      final store = Store(prefs);
      store.claimPairingOwnership();
      final owner = store.pairingOwnership('https://old.invalid');
      await expectLater(
        store.persistPairing(owner, 'https://new.invalid', 'replacement'),
        throwsA(isA<PairingPersistenceFailure>()),
      );
      await prefs.reload();
      expect(store.serverUrl, 'https://old.invalid');
      expect(store.cursor, 12);
      expect(await store.token, 'old-token');
      expect(store.preferencesReliable, isTrue);
    },
  );
  test('rollback secure-write failure stays unpaired and reports uncertain credential', () async {
    SharedPreferences.setMockInitialValues({
      'server': 'https://old.invalid',
      'cursor': 12,
    });
    SharedPreferencesStorePlatform.instance = FailPreferenceOperation(
      'remove',
      throws: true,
    );
    final secure = FailWrite();
    FlutterSecureStoragePlatform.instance = secure;
    final prefs = await SharedPreferences.getInstance();
    final store = Store(prefs);
    store.claimPairingOwnership();
    final owner = store.pairingOwnership('https://old.invalid');
    await expectLater(
      store.persistPairing(owner, 'https://new.invalid', 'replacement'),
      throwsA(isA<PairingPersistenceFailure>()),
    );
    expect(store.serverUrl, 'https://old.invalid');
    expect(await store.token, 'existing-token');
  });
  test('secure credential rollback failure preserves server as attempted owner and does not claim success', () async {
    SharedPreferences.setMockInitialValues({
      'server': 'https://old.invalid',
      'cursor': 12,
    });
    SharedPreferencesStorePlatform.instance = FailPreferenceOperation(
      'remove',
      throws: true,
    );
    FlutterSecureStoragePlatform.instance = FailSecureRollback();
    final prefs = await SharedPreferences.getInstance();
    final store = Store(prefs);
    store.claimPairingOwnership();
    final owner = store.pairingOwnership('https://old.invalid');
    await expectLater(
      store.persistPairing(owner, 'https://new.invalid', 'replacement'),
      throwsA(isA<PairingPersistenceFailure>()),
    );
    expect(store.serverUrl, 'https://new.invalid');
    expect(await store.token, 'replacement');
  });
}
