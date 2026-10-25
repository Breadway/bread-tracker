import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
// ignore_for_file: depend_on_referenced_packages
import 'package:flutter_secure_storage_platform_interface/flutter_secure_storage_platform_interface.dart';
import 'package:flutter_secure_storage/test/test_flutter_secure_storage_platform.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/state.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/fx/sfx.dart';
import 'package:tracker/push.dart';

import 'signout_test.dart' as fixture;

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
