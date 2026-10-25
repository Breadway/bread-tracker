import 'dart:async';
import 'dart:io';

import 'package:drift/drift.dart' show Value;
import 'package:drift/native.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/data/sync.dart';
import 'package:tracker/state.dart';

class MemStore extends Store {
  MemStore(super.prefs);
  String? _t = 'tok';
  Completer<void>? resetStarted;
  Completer<void>? releaseReset;
  @override
  Future<String?> get token async => _t;
  @override
  Future<void> saveToken(String t) async => _t = t;

  @override
  @override
  int credentialEpoch = 0;
  bool pauseConditionalReset = false;

  @override
  void claimCredentialOwnership() => credentialEpoch++;

  @override
  Future<bool> resetSyncIfEpochCurrent(int expectedEpoch) async {
    if (pauseConditionalReset) {
      if (resetStarted != null && !resetStarted!.isCompleted) {
        resetStarted!.complete();
      }
      if (releaseReset != null) await releaseReset!.future;
    }
    if (credentialEpoch != expectedEpoch) return false;
    await super.resetSync();
    return true;
  }

  @override
  Future<bool> clearTokenAndRunIfCurrent(
    String expected,
    Future<void> Function() action,
  ) async {
    if (_t != expected) return false;
    _t = null;
    await action();
    return true;
  }

  @override
  Future<void> clearToken() async => _t = null;

  @override
  Future<bool> runIfTokenless(Future<void> Function() action) async {
    if (_t != null) return false;
    await action();
    return true;
  }

  @override
  Future<bool> clearTokenIfCurrent(String expected) async {
    if (_t != expected) return false;
    _t = null;
    return true;
  }

  @override
  Future<bool> runIfTokenCurrent(
    String expected,
    Future<void> Function() action,
  ) async {
    if (_t != expected) return false;
    if (resetStarted != null && !resetStarted!.isCompleted) {
      resetStarted!.complete();
    }
    if (releaseReset != null) await releaseReset!.future;
    if (_t != expected) return false;
    await action();
    return true;
  }
}

const id1 = '11111111-1111-4111-8111-111111111111';

Future<(ProviderContainer, MemStore, AppDb)> setup(MockClient client) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final store = MemStore(prefs);
  await store.setCursor(12);
  final db = AppDb(NativeDatabase.memory());
  await db.upsertItem(
    ItemsCompanion.insert(
      id: id1,
      kind: 'chore',
      name: 'Dishes',
      points: 10,
      updatedAt: 1,
    ),
  );
  await db.setCompletion(id1, '2026-10-19', true, 2);
  final c = ProviderContainer(
    overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(store),
      dbProvider.overrideWithValue(db),
      apiFactoryProvider.overrideWithValue(
        (base, token) => Api(base, token: token, client: client),
      ),
    ],
  );
  addTearDown(c.dispose);
  return (c, store, db);
}

void main() {
  test('existing file-backed database reopens with retained item and completion history', () async {
    final dir = await Directory.systemTemp.createTemp('track10-db-reopen-');
    addTearDown(() => dir.delete(recursive: true));
    final path = '${dir.path}/tracker.sqlite';
    final first = AppDb(NativeDatabase(File(path)));
    await first.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'legacy install item',
        points: 10,
        updatedAt: 7,
        dirty: const Value(true),
      ),
    );
    await first.setCompletion(id1, '2026-10-19', true, 8);
    await first.close();

    final reopened = AppDb(NativeDatabase(File(path)));
    expect(
      (await reopened.select(reopened.items).getSingle()).name,
      'legacy install item',
    );
    expect(
      (await reopened.select(reopened.completions).getSingle()).completedAt,
      8,
    );
    await reopened.close();
  });

  test('sign out revokes on the server, forgets the token and cursor, keeps data by default', () async {
    final calls = <String>[];
    final (c, store, db) = await setup(
      MockClient((r) async {
        calls.add('${r.method} ${r.url.path} ${r.headers['authorization']}');
        return http.Response('{"ok":true}', 200);
      }),
    );
    expect(await c.read(controllerProvider).signOut(), isTrue);
    expect(calls, ['DELETE /device Bearer tok']);
    expect(await store.token, isNull);
    expect(store.cursor, 0);
    expect(await db.select(db.items).get(), hasLength(1));
    expect(await db.select(db.completions).get(), hasLength(1));
  });

  test('sign out can wipe local data', () async {
    final (c, store, db) = await setup(
      MockClient((_) async => http.Response('{}', 200)),
    );
    expect(await c.read(controllerProvider).signOut(wipeLocal: true), isTrue);
    expect(await store.token, isNull);
    expect(await db.select(db.items).get(), isEmpty);
    expect(await db.select(db.completions).get(), isEmpty);
  });

  test('sign out still signs out locally when the server is unreachable, and says so', () async {
    final (c, store, _) = await setup(
      MockClient((_) async => throw http.ClientException('offline')),
    );
    expect(
      await c.read(controllerProvider).signOut(),
      isFalse,
      reason: 'caller is told the token may still be valid',
    );
    expect(await store.token, isNull);
  });

  test('sign out of an already revoked device counts as revoked', () async {
    final (c, store, _) = await setup(
      MockClient((_) async => http.Response('{"error":"unauthorized"}', 401)),
    );
    expect(await c.read(controllerProvider).signOut(), isTrue);
    expect(await store.token, isNull);
  });

  test(
    'sign out invalidates an in-flight sync before it can restore wiped data',
    () async {
      final entered = Completer<void>();
      final release = Completer<void>();
      SharedPreferences.setMockInitialValues({});
      final prefs = await SharedPreferences.getInstance();
      final store = MemStore(prefs);
      final db = AppDb(NativeDatabase.memory());
      await db.upsertItem(
        ItemsCompanion.insert(
          id: id1,
          kind: 'chore',
          name: 'private',
          points: 10,
          updatedAt: 1,
          dirty: const Value(true),
        ),
      );
      final container = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(prefs),
          storeProvider.overrideWithValue(store),
          dbProvider.overrideWithValue(db),
          apiFactoryProvider.overrideWithValue(
            (base, token) => Api(
              base,
              token: token,
              client: MockClient((r) async {
                if (r.method == 'POST') {
                  entered.complete();
                  await release.future;
                  return http.Response(
                    '{"cursor":17,"items":[],"completions":[]}',
                    200,
                  );
                }
                return http.Response('{"ok":true}', 200);
              }),
            ),
          ),
        ],
      );
      addTearDown(container.dispose);
      final syncing = container.read(syncEngineProvider).sync();
      await entered.future;
      await container.read(controllerProvider).signOut(wipeLocal: true);
      release.complete();
      expect(await syncing, SyncStatus.unpaired);
      expect(await db.select(db.items).get(), isEmpty);
      expect(store.cursor, 0);
    },
  );

  test('keeping data on sign out still rejects an old response', () async {
    final entered = Completer<void>();
    final release = Completer<void>();
    SharedPreferences.setMockInitialValues({});
    final prefs = await SharedPreferences.getInstance();
    final store = MemStore(prefs);
    final db = AppDb(NativeDatabase.memory());
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'keep',
        points: 10,
        updatedAt: 1,
        dirty: const Value(true),
      ),
    );
    final container = ProviderContainer(
      overrides: [
        prefsProvider.overrideWithValue(prefs),
        storeProvider.overrideWithValue(store),
        dbProvider.overrideWithValue(db),
        apiFactoryProvider.overrideWithValue(
          (base, token) => Api(
            base,
            token: token,
            client: MockClient((r) async {
              if (r.method == 'POST') {
                entered.complete();
                await release.future;
                return http.Response(
                  '{"cursor":17,"items":[],"completions":[]}',
                  200,
                );
              }
              return http.Response('{"ok":true}', 200);
            }),
          ),
        ),
      ],
    );
    addTearDown(container.dispose);
    final sync = container.read(syncEngineProvider).sync();
    await entered.future;
    await container.read(controllerProvider).signOut();
    release.complete();
    expect(await sync, SyncStatus.unpaired);
    expect((await db.select(db.items).getSingle()).name, 'keep');
  });

  test(
    're-pair invalidates an old response without clearing the new credential',
    () async {
      final entered = Completer<void>();
      final release = Completer<void>();
      SharedPreferences.setMockInitialValues({});
      final prefs = await SharedPreferences.getInstance();
      final store = MemStore(prefs);
      final db = AppDb(NativeDatabase.memory());
      await db.upsertItem(
        ItemsCompanion.insert(
          id: id1,
          kind: 'chore',
          name: 'private',
          points: 10,
          updatedAt: 1,
          dirty: const Value(true),
        ),
      );
      final container = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(prefs),
          storeProvider.overrideWithValue(store),
          dbProvider.overrideWithValue(db),
          apiFactoryProvider.overrideWithValue(
            (base, token) => Api(
              base,
              token: token,
              client: MockClient((r) async {
                if (r.method == 'POST' && !entered.isCompleted) {
                  entered.complete();
                  await release.future;
                }
                return http.Response(
                  '{"cursor":17,"items":[],"completions":[]}',
                  200,
                );
              }),
            ),
          ),
        ],
      );
      addTearDown(container.dispose);
      final syncing = container.read(syncEngineProvider).sync();
      await entered.future;
      final ctl = container.read(controllerProvider);
      await ctl.signOut();
      await store.saveToken('new-token');
      ctl.pairedAgain();
      release.complete();
      expect(await syncing, SyncStatus.unpaired);
      expect(await store.token, 'new-token');
      expect(store.cursor, 0);
      expect((await db.select(db.items).getSingle()).dirty, isTrue);
    },
  );

  test(
    'delayed successful revoke cannot clear replacement token or wipe its data',
    () async {
      final enteredDelete = Completer<void>();
      final releaseDelete = Completer<void>();
      final client = MockClient((r) async {
        if (r.method == 'DELETE') {
          enteredDelete.complete();
          await releaseDelete.future;
        }
        return http.Response('{"ok":true}', 200);
      });
      final (container, store, db) = await setup(client);
      final signingOut = container
          .read(controllerProvider)
          .signOut(wipeLocal: true);
      await enteredDelete.future;

      container.read(controllerProvider).pairedAgain();
      final replacementWrite = store.saveToken('replacement');
      await replacementWrite;
      await db.upsertItem(
        ItemsCompanion.insert(
          id: '22222222-2222-4222-8222-222222222222',
          kind: 'habit',
          name: 'new device data',
          points: 15,
          updatedAt: 3,
          dirty: const Value(true),
        ),
      );
      releaseDelete.complete();

      expect(await signingOut, isTrue);
      expect(await store.token, 'replacement');
      expect(
        (await db.select(db.items).get()).map((r) => r.name),
        containsAll(['Dishes', 'new device data']),
      );
      expect(store.cursor, 12);
    },
  );

  test(
    'overlapping sign-outs allow only newest generation to mutate local state',
    () async {
      final enteredFirstDelete = Completer<void>();
      final releaseFirstDelete = Completer<void>();
      var deleteCount = 0;
      final (container, store, db) = await setup(
        MockClient((request) async {
          if (request.method == 'DELETE' && ++deleteCount == 1) {
            enteredFirstDelete.complete();
            await releaseFirstDelete.future;
          }
          return http.Response('{"ok":true}', 200);
        }),
      );
      final controller = container.read(controllerProvider);
      final first = controller.signOut(wipeLocal: true);
      await enteredFirstDelete.future;
      final second = controller.signOut();
      expect(await second, isTrue);
      releaseFirstDelete.complete();
      expect(await first, isTrue);
      expect(await store.token, isNull);
      expect(await db.select(db.items).get(), hasLength(1));
    },
  );

  test(
    'replacement at database wipe boundary retains rows and cursor',
    () async {
      final (container, store, db) = await setup(
        MockClient((_) async => http.Response('{"ok":true}', 200)),
      );
      final signingOut = container
          .read(controllerProvider)
          .signOut(wipeLocal: true);
      final resetStarted = Completer<void>();
      store.resetStarted = resetStarted;
      store.releaseReset = Completer<void>();
      store.pauseConditionalReset = true;
      await resetStarted.future;
      container.read(controllerProvider).pairedAgain();
      final replacementWrite = store.saveToken('replacement');
      store.releaseReset!.complete();

      expect(await signingOut, isTrue);
      await replacementWrite;
      expect(await store.token, 'replacement');
      expect(store.cursor, 12);
      expect(await db.select(db.items).get(), hasLength(1));
      expect(await db.select(db.completions).get(), hasLength(1));
    },
  );
}
