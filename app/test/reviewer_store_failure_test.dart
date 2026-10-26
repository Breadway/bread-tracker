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
import 'package:tracker/data/api.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/pair.dart';
import 'package:tracker/fx/sfx.dart';
import 'package:tracker/push.dart';
import 'package:tracker/app.dart';
import 'package:tracker/ui/settings.dart';

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

  testWidgets(
    'real app bootstrap exposes Settings sign out from installed token',
    (tester) async {
      SharedPreferences.setMockInitialValues({'server': 'https://old.invalid'});
      FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({
        'token': 'installed-token',
      });
      final prefs = await SharedPreferences.getInstance();
      final store = Store(prefs);
      final client = MockClient((request) async => http.Response('{}', 200));
      final container = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(prefs),
          storeProvider.overrideWithValue(store),
          dbProvider.overrideWithValue(AppDb(NativeDatabase.memory())),
          apiFactoryProvider.overrideWithValue(
            (base, token) => Api(base, token: token, client: client),
          ),
          sfxProvider.overrideWithValue(NoSfx()),
          pushProvider.overrideWithValue(NoPushService()),
        ],
      );
      addTearDown(container.dispose);
      final prior = Api.sharedClient;
      Api.sharedClient = client;
      addTearDown(() => Api.sharedClient = prior);
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: container,
          child: const MaterialApp(home: Scaffold(body: SettingsScreen())),
        ),
      );
      await tester.pump();
      expect(identical(container.read(storeProvider), store), isTrue);
      expect(await container.read(pairedProvider.future), isTrue);
      await tester.pump();
      expect(find.text('Sign out'), findsOneWidget);
    },
  );
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
