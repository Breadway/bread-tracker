import 'package:drift/native.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/state.dart';

class MemStore extends Store {
  MemStore(super.prefs);
  String? _t = 'tok';
  @override
  Future<String?> get token async => _t;
  @override
  Future<void> saveToken(String t) async => _t = t;
  @override
  Future<void> clearToken() async => _t = null;
}

const id1 = '11111111-1111-4111-8111-111111111111';

Future<(ProviderContainer, MemStore, AppDb)> setup(MockClient client) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final store = MemStore(prefs);
  await store.setCursor(12);
  final db = AppDb(NativeDatabase.memory());
  await db.upsertItem(ItemsCompanion.insert(id: id1, kind: 'chore', name: 'Dishes', points: 10, updatedAt: 1));
  await db.setCompletion(id1, '2026-10-19', true, 2);
  final c = ProviderContainer(overrides: [
    prefsProvider.overrideWithValue(prefs),
    storeProvider.overrideWithValue(store),
    dbProvider.overrideWithValue(db),
    apiFactoryProvider.overrideWithValue((base, token) => Api(base, token: token, client: client)),
  ]);
  addTearDown(c.dispose);
  return (c, store, db);
}

void main() {
  test('sign out revokes on the server, forgets the token and cursor, keeps data by default', () async {
    final calls = <String>[];
    final (c, store, db) = await setup(MockClient((r) async {
      calls.add('${r.method} ${r.url.path} ${r.headers['authorization']}');
      return http.Response('{"ok":true}', 200);
    }));
    expect(await c.read(controllerProvider).signOut(), isTrue);
    // The account is looked up first (best effort) so kept data stays attributed to it.
    expect(calls, ['GET /me Bearer tok', 'DELETE /device Bearer tok']);
    expect(await store.token, isNull);
    expect(store.cursor, 0);
    expect(await db.select(db.items).get(), hasLength(1));
    expect(await db.select(db.completions).get(), hasLength(1));
  });

  test('sign out can wipe local data', () async {
    final (c, store, db) = await setup(MockClient((_) async => http.Response('{}', 200)));
    expect(await c.read(controllerProvider).signOut(wipeLocal: true), isTrue);
    expect(await store.token, isNull);
    expect(await db.select(db.items).get(), isEmpty);
    expect(await db.select(db.completions).get(), isEmpty);
  });

  test('sign out still signs out locally when the server is unreachable, and says so', () async {
    final (c, store, _) = await setup(MockClient((_) async => throw http.ClientException('offline')));
    expect(await c.read(controllerProvider).signOut(), isFalse, reason: 'caller is told the token may still be valid');
    expect(await store.token, isNull);
  });

  test('sign out of an already revoked device counts as revoked', () async {
    final (c, store, _) = await setup(MockClient((_) async => http.Response('{"error":"unauthorized"}', 401)));
    expect(await c.read(controllerProvider).signOut(), isTrue);
    expect(await store.token, isNull);
  });
}
