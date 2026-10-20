import 'package:drift/drift.dart' show Value, driftRuntimeOptions;
import 'package:drift/native.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/data/sync.dart';

/// A tiny in-memory stand-in for the server's last-write-wins sync.
class FakeServer {
  int seq = 0;
  final items = <String, Map<String, dynamic>>{};
  final comps = <String, Map<String, dynamic>>{};
  bool reject = false;
  bool down = false;
  String epoch = 'epoch-1';

  /// A rebuilt server: empty database, new identity, counter back at zero.
  void rebuild(String newEpoch) {
    seq = 0;
    items.clear();
    comps.clear();
    epoch = newEpoch;
  }

  Map<String, dynamic> sync(int since, List<Map<String, dynamic>> pi, List<Map<String, dynamic>> pc) {
    if (down) throw ApiException('Cannot reach the server');
    if (reject) throw AuthLost();
    for (final i in pi) {
      final e = items[i['id']];
      if (e != null && e['updated_at'] >= i['updated_at']) continue;
      items[i['id']] = {...i, 'seq': ++seq};
    }
    for (final c in pc) {
      final k = '${c['item_id']}|${c['date']}';
      final e = comps[k];
      if (e != null && e['updated_at'] >= c['updated_at']) continue;
      comps[k] = {...c, 'seq': ++seq};
    }
    // Mirror the real server: a cursor ahead of the database means "send everything".
    if (since > seq) since = 0;
    return {
      'epoch': epoch,
      'cursor': seq,
      'items': items.values.where((i) => i['seq'] > since).toList(),
      'completions': comps.values.where((c) => c['seq'] > since).toList(),
      'server_time': 0,
    };
  }
}

class FakeApi extends Api {
  FakeApi(this.server) : super('http://fake', token: 't');
  final FakeServer server;
  @override
  Future<Map<String, dynamic>> sync(int since, List<Map<String, dynamic>> items, List<Map<String, dynamic>> completions) async =>
      server.sync(since, items, completions);
}

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

Future<(AppDb, SyncEngine, MemStore)> device(FakeServer s) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final db = AppDb(NativeDatabase.memory());
  final store = MemStore(prefs);
  return (db, SyncEngine(db, store, apiFactory: (_, _) => FakeApi(s)), store);
}

void main() {
  driftRuntimeOptions.dontWarnAboutMultipleDatabases = true;
  test('two offline devices converge, last write wins, un-tick syncs', () async {
    final server = FakeServer();
    final (dbA, syncA, _) = await device(server);
    final (dbB, syncB, _) = await device(server);

    // Phone A creates a chore and ticks it while offline.
    await dbA.upsertItem(ItemsCompanion.insert(id: id1, kind: 'chore', name: 'Dishes', points: 10, updatedAt: 1000, dirty: const Value(true)));
    await dbA.setCompletion(id1, '2026-10-19', true, 2000);
    expect(await syncA.sync(), SyncStatus.idle);
    expect(server.items.length, 1);
    expect(server.comps.length, 1);

    // Device B pulls everything.
    expect(await syncB.sync(), SyncStatus.idle);
    expect((await dbB.select(dbB.items).get()).single.name, 'Dishes');
    expect((await dbB.select(dbB.completions).get()).single.deletedAt, isNull);

    // B un-ticks later; A renames earlier (stale). Both offline, then sync.
    await dbB.setCompletion(id1, '2026-10-19', false, 9000);
    await dbB.upsertItem(ItemsCompanion(id: const Value(id1), kind: const Value('chore'), name: const Value('Dishes B'), points: const Value(10), updatedAt: const Value(3000), dirty: const Value(true)));
    await dbA.upsertItem(ItemsCompanion(id: const Value(id1), kind: const Value('chore'), name: const Value('Stale name'), points: const Value(10), updatedAt: const Value(1500), dirty: const Value(true)));
    await syncB.sync();
    await syncA.sync();

    final a = await dbA.select(dbA.completions).getSingle();
    expect(a.deletedAt, isNotNull, reason: 'the un-tick reached A');
    expect((await dbA.select(dbA.items).getSingle()).name, 'Dishes B', reason: 'the older rename from A lost to the newer one from B');
    // Nothing is left dirty once converged.
    expect(await (dbA.select(dbA.items)..where((t) => t.dirty.equals(true))).get(), isEmpty);
    expect(await (dbB.select(dbB.completions)..where((t) => t.dirty.equals(true))).get(), isEmpty);
  });

  test('offline keeps rows dirty; auth loss reports unpaired', () async {
    final server = FakeServer()..down = true;
    final (db, sync, _) = await device(server);
    await db.upsertItem(ItemsCompanion.insert(id: id1, kind: 'habit', name: 'Read', points: 15, updatedAt: 1, dirty: const Value(true)));
    expect(await sync.sync(), SyncStatus.offline);
    expect(await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(), hasLength(1));

    server
      ..down = false
      ..reject = true;
    expect(await sync.sync(), SyncStatus.unpaired);

    server.reject = false;
    expect(await sync.sync(), SyncStatus.idle);
    expect(await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(), isEmpty);
  });

  test('non-JSON replies (HTML 502, captive portal 200) report offline, not a crash', () async {
    for (final (code, body) in [
      (502, '<html><body>Bad gateway</body></html>'),
      (503, 'upstream down'),
      (200, '<html>captive portal</html>'),
      (200, '[1, 2, 3]'),
    ]) {
      SharedPreferences.setMockInitialValues({});
      final prefs = await SharedPreferences.getInstance();
      final db = AppDb(NativeDatabase.memory());
      final store = MemStore(prefs);
      final client = MockClient((_) async => http.Response(body, code, headers: {'content-type': 'text/html'}));
      final engine = SyncEngine(db, store, apiFactory: (b, t) => Api(b, token: t, client: client));
      await db.upsertItem(ItemsCompanion.insert(id: id1, kind: 'habit', name: 'Read', points: 15, updatedAt: 1, dirty: const Value(true)));
      expect(await engine.sync(), SyncStatus.offline, reason: '$code $body');
      expect(await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(), hasLength(1), reason: 'row stays dirty');
      await db.close();
    }
  });

  test('Api maps a JSON error body and an HTML error body to ApiException', () async {
    final json = Api('http://x', token: 't', client: MockClient((_) async => http.Response('{"error":"nope"}', 400)));
    await expectLater(json.pushTest(), throwsA(isA<ApiException>().having((e) => e.message, 'message', 'nope')));
    final html = Api('http://x', token: 't', client: MockClient((_) async => http.Response('<html/>', 502)));
    await expectLater(html.pushTest(), throwsA(isA<ApiException>().having((e) => e.message, 'message', 'Server error 502')));
  });

  test('a rebuilt server (new epoch) receives everything the phone already had', () async {
    final server = FakeServer();
    final (db, sync, store) = await device(server);
    await db.upsertItem(ItemsCompanion.insert(id: id1, kind: 'chore', name: 'Dishes', points: 10, updatedAt: 1000, dirty: const Value(true)));
    await db.setCompletion(id1, '2026-10-19', true, 2000);
    expect(await sync.sync(), SyncStatus.idle);
    expect(store.epoch, 'epoch-1');
    expect(await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(), isEmpty, reason: 'clean after the first sync');

    // The server is lost and rebuilt empty; its counter starts again from zero.
    server.rebuild('epoch-2');
    expect(await sync.sync(), SyncStatus.idle);
    expect(server.epoch, 'epoch-2');
    expect(server.items.keys, [id1], reason: 'the item was pushed again');
    expect(server.comps.length, 1, reason: 'and so was the completion');
    expect(store.epoch, 'epoch-2');
    expect(store.cursor, server.seq);
    expect(await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(), isEmpty);
  });

  test('a restored older copy (same epoch, cursor went backwards) is repopulated and read again', () async {
    final server = FakeServer();
    final (db, sync, store) = await device(server);
    for (var i = 0; i < 3; i++) {
      final id = '1111111$i-1111-4111-8111-111111111111';
      await db.upsertItem(ItemsCompanion.insert(id: id, kind: 'chore', name: 'c$i', points: 10, updatedAt: 1000 + i, dirty: const Value(true)));
    }
    await sync.sync();
    expect(store.cursor, 3);

    // Restore from a replica that only had the first item: counter goes back to 1, same epoch.
    final keep = server.items.entries.first;
    server.items
      ..clear()
      ..[keep.key] = {...keep.value, 'seq': 1};
    server.seq = 1;
    expect(await sync.sync(), SyncStatus.idle);
    expect(server.items.length, 3, reason: 'rows missing from the restored copy came back from the phone');
    expect(store.cursor, server.seq);

    // Rows written elsewhere after the restore are seen even though their seq is below the old cursor.
    server.items['22222222-2222-4222-8222-222222222222'] = {
      'id': '22222222-2222-4222-8222-222222222222', 'kind': 'chore', 'name': 'from B', 'weekdays': 127,
      'remind_time': null, 'points': 10, 'due_date': null, 'sort': 0, 'active': true,
      'updated_at': 5000, 'deleted_at': null, 'seq': ++server.seq,
    };
    await sync.sync();
    expect((await db.select(db.items).get()).map((i) => i.name), contains('from B'));
  });

  test('an unchanged server does not trigger a full re-push', () async {
    final server = FakeServer();
    final (db, sync, _) = await device(server);
    await db.upsertItem(ItemsCompanion.insert(id: id1, kind: 'chore', name: 'Dishes', points: 10, updatedAt: 1000, dirty: const Value(true)));
    await sync.sync();
    final seqAfterFirst = server.seq;
    await sync.sync();
    await sync.sync();
    expect(server.seq, seqAfterFirst, reason: 'nothing was pushed again');
  });

  test('pairing marks all local rows for a full push', () async {
    final server = FakeServer();
    final (db, _, store) = await device(server);
    await db.upsertItem(ItemsCompanion.insert(id: id1, kind: 'chore', name: 'Dishes', points: 10, updatedAt: 1000));
    await db.setCompletion(id1, '2026-10-19', true, 2000);
    await (db.update(db.completions)).write(const CompletionsCompanion(dirty: Value(false)));
    await store.setCursor(40);
    await store.setEpoch('old');
    // This is what the pairing screen does after a successful pair.
    await store.resetSync();
    await db.markAllDirty();
    expect(store.cursor, 0);
    expect(store.epoch, isNull);
    expect(await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(), hasLength(1));
    expect(await (db.select(db.completions)..where((t) => t.dirty.equals(true))).get(), hasLength(1));
  });

  test('default Api instances share one http client instead of opening one per call', () async {
    final seen = <String>[];
    final shared = MockClient((r) async {
      seen.add('${r.method} ${r.url.path}');
      return http.Response('{"cursor": 0, "items": [], "completions": [], "server_time": 0}', 200);
    });
    Api.sharedClient = shared;
    addTearDown(() => Api.sharedClient = null);

    // Two separately built Apis (different base/token) use the same client.
    await Api('http://a', token: 't1').sync(0, [], []);
    await Api('http://b', token: 't2').sync(0, [], []);
    expect(seen, ['POST /sync', 'POST /sync']);

    // A default SyncEngine builds an Api per pass; every pass still goes through it.
    SharedPreferences.setMockInitialValues({});
    final db = AppDb(NativeDatabase.memory());
    final engine = SyncEngine(db, MemStore(await SharedPreferences.getInstance()));
    for (var i = 0; i < 3; i++) {
      expect(await engine.sync(), SyncStatus.idle);
    }
    expect(seen.length, 5);
    await db.close();

    // An injected client is used instead of the shared one.
    final own = MockClient((r) async => http.Response('{}', 200));
    await Api('http://c', token: 't', client: own).sync(0, [], []);
    expect(seen.length, 5);
  });

  test('an edit made while a sync is in flight stays dirty', () async {
    final server = FakeServer();
    final (db, sync, _) = await device(server);
    await db.upsertItem(ItemsCompanion.insert(id: id1, kind: 'chore', name: 'A', points: 10, updatedAt: 100, dirty: const Value(true)));
    await sync.sync();
    // Local edit newer than what the server has; a pull that echoes the old row must not overwrite it.
    await db.upsertItem(ItemsCompanion(id: const Value(id1), kind: const Value('chore'), name: const Value('B'), points: const Value(10), updatedAt: const Value(200), dirty: const Value(true)));
    server.items[id1] = {...server.items[id1]!, 'seq': ++server.seq};
    await sync.sync();
    expect((await db.select(db.items).getSingle()).name, 'B');
    expect(server.items[id1]!['name'], 'B');
  });
}
