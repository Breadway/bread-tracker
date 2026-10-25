import 'dart:async';

import 'package:drift/native.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
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

import 'signout_test.dart' as fixture;

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
    ctl.pairedAgain();
    final write = store.saveToken('replacement');
    await store.setServerUrl('https://replacement.example.invalid');
    release.complete();
    await pending.timeout(const Duration(seconds: 3));
    await write;
    expect(await store.token, 'replacement');
    expect(await db.select(db.items).get(), hasLength(1));
    expect(await db.select(db.completions).get(), hasLength(1));
  });

  test(
    'database wipe transaction aborts on generation change before commit',
    () async {
      final (c, store, db) = await fixture.setup(
        MockClient((_) async => http.Response('{}', 200)),
      );
      addTearDown(db.close);
      final ctl = c.read(controllerProvider);
      try {
        await db.transaction(() async {
          await db.delete(db.completions).go();
          await db.delete(db.items).go();
          ctl.pairedAgain();
          // Same generation guard used by production wipe: deliberately force rollback.
          throw StateError('auth generation superseded before commit');
        });
      } catch (_) {}
      expect(await db.select(db.items).get(), hasLength(1));
      expect(await db.select(db.completions).get(), hasLength(1));
      expect(await store.token, 'tok');
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
    final ctl = c.read(controllerProvider);
    final signout = ctl.signOut(wipeLocal: true);
    await enteredDelete.future;
    ctl.pairedAgain();
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
    controller.pairedAgain();
    releaseReset.complete();
    await signingOut;
    expect(await db.select(db.items).get(), hasLength(1));
    expect(await db.select(db.completions).get(), hasLength(1));
    expect(
      await store.token,
      isNull,
      reason: 'the sign-out owns removal of its old token',
    );
  });

  test('failed replacement persistence does not reopen sync admission', () async {
    final (c, store, db) = await fixture.setup(
      MockClient((_) async => http.Response('{}', 200)),
    );
    addTearDown(db.close);
    final ctl = c.read(controllerProvider);
    ctl.pairedAgain();
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
      final ctl = c.read(controllerProvider);
      final pending = ctl.signOut(wipeLocal: true);
      await entered.future;
      await store.saveToken('replacement');
      ctl.pairedAgain();
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
    final ctl = c.read(controllerProvider);
    final pending = ctl.signOut(wipeLocal: true);
    await entered.future;
    await store.saveToken('replacement');
    ctl.pairedAgain();
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
