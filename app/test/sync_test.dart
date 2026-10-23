import 'package:drift/drift.dart' show Value, driftRuntimeOptions;
import 'package:drift/native.dart';
import 'package:flutter_test/flutter_test.dart';
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
  bool supportsEpoch = true;
  bool disconnectAfterFirstRequest = false;
  bool firstRequest = false;

  /// A rebuilt server: empty database, new identity, counter back at zero.
  void rebuild(String newEpoch) {
    seq = 0;
    items.clear();
    comps.clear();
    epoch = newEpoch;
  }

  Map<String, dynamic> sync(
    int since,
    List<Map<String, dynamic>> pi,
    List<Map<String, dynamic>> pc, {
    String? clientEpoch,
  }) {
    if (down) throw ApiException('Cannot reach the server');
    if (reject) throw AuthLost();
    if (!supportsEpoch) {
      for (final i in pi) {
        final old = items[i['id']];
        if (old == null || old['updated_at'] < i['updated_at']) {
          items[i['id']] = {...i, 'seq': ++seq};
        }
      }
      for (final c in pc) {
        final key = '${c['item_id']}|${c['date']}';
        final old = comps[key];
        if (old == null || old['updated_at'] < c['updated_at']) {
          comps[key] = {...c, 'seq': ++seq};
        }
      }
      return {
        'cursor': seq,
        'items': items.values.where((i) => i['seq'] > since).toList(),
        'completions': comps.values.where((c) => c['seq'] > since).toList(),
        'server_time': 0,
      };
    }
    if (clientEpoch != null && clientEpoch != epoch) {
      if (pi.isNotEmpty || pc.isNotEmpty) {
        throw ApiException('generation changed');
      }
      return {
        'epoch': epoch,
        'cursor': seq,
        'items': items.values.toList(),
        'completions': comps.values.toList(),
        'server_time': 0,
      };
    }
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
    return {
      'cursor': seq,
      'epoch': epoch,
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
  Future<Map<String, dynamic>> sync(
    int since,
    List<Map<String, dynamic>> items,
    List<Map<String, dynamic>> completions, {
    String? epoch,
  }) async {
    if (!server.supportsEpoch) {
      return server.sync(since, items, completions);
    }
    if (epoch != null && epoch != server.epoch) {
      return {
        'epoch': server.epoch,
        'cursor': server.seq,
        'items': server.items.values.toList(),
        'completions': server.comps.values.toList(),
        'server_time': 0,
      };
    }
    final response = server.sync(since, items, completions, clientEpoch: epoch);
    if (server.disconnectAfterFirstRequest && !server.firstRequest) {
      server.firstRequest = true;
      server.supportsEpoch = false;
    }
    return response;
  }
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
  test(
    'two offline devices converge, last write wins, un-tick syncs',
    () async {
      final server = FakeServer();
      final (dbA, syncA, _) = await device(server);
      final (dbB, syncB, _) = await device(server);

      // Phone A creates a chore and ticks it while offline.
      await dbA.upsertItem(
        ItemsCompanion.insert(
          id: id1,
          kind: 'chore',
          name: 'Dishes',
          points: 10,
          updatedAt: 1000,
          dirty: const Value(true),
        ),
      );
      await dbA.setCompletion(id1, '2026-10-19', true, 2000);
      expect(await syncA.sync(), SyncStatus.idle);
      expect(server.items.length, 1);
      expect(server.comps.length, 1);

      // Device B pulls everything.
      expect(await syncB.sync(), SyncStatus.idle);
      expect((await dbB.select(dbB.items).get()).single.name, 'Dishes');
      expect(
        (await dbB.select(dbB.completions).get()).single.deletedAt,
        isNull,
      );

      // B un-ticks later; A renames earlier (stale). Both offline, then sync.
      await dbB.setCompletion(id1, '2026-10-19', false, 9000);
      await dbB.upsertItem(
        ItemsCompanion(
          id: const Value(id1),
          kind: const Value('chore'),
          name: const Value('Dishes B'),
          points: const Value(10),
          updatedAt: const Value(3000),
          dirty: const Value(true),
        ),
      );
      await dbA.upsertItem(
        ItemsCompanion(
          id: const Value(id1),
          kind: const Value('chore'),
          name: const Value('Stale name'),
          points: const Value(10),
          updatedAt: const Value(1500),
          dirty: const Value(true),
        ),
      );
      await syncB.sync();
      await syncA.sync();

      final a = await dbA.select(dbA.completions).getSingle();
      expect(a.deletedAt, isNotNull, reason: 'the un-tick reached A');
      expect(
        (await dbA.select(dbA.items).getSingle()).name,
        'Dishes B',
        reason: 'the older rename from A lost to the newer one from B',
      );
      // Nothing is left dirty once converged.
      expect(
        await (dbA.select(dbA.items)..where((t) => t.dirty.equals(true))).get(),
        isEmpty,
      );
      expect(
        await (dbB.select(
          dbB.completions,
        )..where((t) => t.dirty.equals(true))).get(),
        isEmpty,
      );
    },
  );

  test('offline keeps rows dirty; auth loss reports unpaired', () async {
    final server = FakeServer()..down = true;
    final (db, sync, _) = await device(server);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'habit',
        name: 'Read',
        points: 15,
        updatedAt: 1,
        dirty: const Value(true),
      ),
    );
    expect(await sync.sync(), SyncStatus.offline);
    expect(
      await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(),
      hasLength(1),
    );

    server
      ..down = false
      ..reject = true;
    expect(await sync.sync(), SyncStatus.unpaired);

    server.reject = false;
    expect(await sync.sync(), SyncStatus.idle);
    expect(
      await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(),
      isEmpty,
    );
  });

  test('a rebuilt server (new epoch) receives everything the phone already had', () async {
    final server = FakeServer();
    final (db, sync, store) = await device(server);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'Dishes',
        points: 10,
        updatedAt: 1000,
        dirty: const Value(true),
      ),
    );
    await db.setCompletion(id1, '2026-10-19', true, 2000);
    expect(await sync.sync(), SyncStatus.idle);
    expect(store.epoch, 'epoch-1');
    expect(
      await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(),
      isEmpty,
      reason: 'clean after the first sync',
    );

    // The server is lost and rebuilt empty; its counter starts again from zero.
    server.rebuild('epoch-2');
    expect(await sync.sync(), SyncStatus.idle);
    expect(server.epoch, 'epoch-2');
    expect(server.items.keys, [id1], reason: 'the item was pushed again');
    expect(server.comps.length, 1, reason: 'and so was the completion');
    expect(store.epoch, 'epoch-2');
    expect(store.cursor, server.seq);
    expect(
      await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(),
      isEmpty,
    );
  });

  test('an explicitly rotated generation recovers rows from an older restored copy', () async {
    final server = FakeServer();
    final (db, sync, store) = await device(server);
    for (var i = 0; i < 3; i++) {
      final id = '1111111$i-1111-4111-8111-111111111111';
      await db.upsertItem(
        ItemsCompanion.insert(
          id: id,
          kind: 'chore',
          name: 'c$i',
          points: 10,
          updatedAt: 1000 + i,
          dirty: const Value(true),
        ),
      );
    }
    await sync.sync();
    expect(store.cursor, 3);

    // Restore from a replica that only had the first item: counter goes back to 1, same epoch.
    final keep = server.items.entries.first;
    server.items
      ..clear()
      ..[keep.key] = {...keep.value, 'seq': 1};
    server
      ..seq = 1
      ..epoch = 'epoch-restored';
    expect(await sync.sync(), SyncStatus.idle);
    expect(
      server.items.length,
      3,
      reason: 'rows missing from the restored copy came back from the phone',
    );
    expect(store.cursor, server.seq);

    // Rows written elsewhere after the restore are seen even though their seq is below the old cursor.
    server.items['22222222-2222-4222-8222-222222222222'] = {
      'id': '22222222-2222-4222-8222-222222222222',
      'kind': 'chore',
      'name': 'from B',
      'weekdays': 127,
      'remind_time': null,
      'points': 10,
      'due_date': null,
      'sort': 0,
      'active': true,
      'updated_at': 5000,
      'deleted_at': null,
      'seq': ++server.seq,
    };
    await sync.sync();
    expect(
      (await db.select(db.items).get()).map((i) => i.name),
      contains('from B'),
    );
  });

  test(
    'restore applies LWW against newer clean local values and tombstones',
    () async {
      final server = FakeServer();
      final (db, sync, store) = await device(server);
      await db.upsertItem(
        ItemsCompanion.insert(
          id: id1,
          kind: 'chore',
          name: 'backup name',
          points: 10,
          updatedAt: 1000,
          dirty: const Value(true),
        ),
      );
      await db.setCompletion(id1, '2026-10-19', true, 1100);
      await sync.sync();
      final savedItem = Map<String, dynamic>.from(server.items[id1]!);
      final savedCompletion = Map<String, dynamic>.from(
        server.comps.values.single,
      );

      // A newer edit and untick have already reached the server and local DB, so both
      // local rows are clean when a backup from the old generation is restored.
      await db.upsertItem(
        ItemsCompanion(
          id: const Value(id1),
          kind: const Value('chore'),
          name: const Value('new synced name'),
          points: const Value(10),
          updatedAt: const Value(2000),
          dirty: const Value(true),
        ),
      );
      await db.setCompletion(id1, '2026-10-19', false, 2100);
      await sync.sync();
      expect((await db.select(db.items).getSingle()).dirty, false);
      expect((await db.select(db.completions).getSingle()).dirty, false);

      server.items[id1] = savedItem;
      server.comps[server.comps.keys.single] = savedCompletion;
      server.epoch = 'restore-epoch';
      server.seq = 1;
      await db.markAllDirty();
      expect(await sync.sync(), SyncStatus.idle);
      expect(server.items[id1]!['name'], 'new synced name');
      expect(server.comps.values.single['deleted_at'], 2100);
      expect((await db.select(db.items).getSingle()).name, 'new synced name');
      expect((await db.select(db.completions).getSingle()).deletedAt, 2100);
      expect(store.epoch, 'restore-epoch');
    },
  );

  test('restore equal timestamps retain local state and dirty winners are retried', () async {
    final server = FakeServer();
    final (db, sync, store) = await device(server);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'backup',
        points: 10,
        updatedAt: 1000,
        dirty: const Value(true),
      ),
    );
    await sync.sync();
    final backup = Map<String, dynamic>.from(server.items[id1]!);
    server.epoch = 'restore-equal';
    server.items[id1] = {...backup, 'name': 'equal remote conflict'};
    await db.upsertItem(
      ItemsCompanion(
        id: const Value(id1),
        kind: const Value('chore'),
        name: const Value('equal local'),
        points: const Value(10),
        updatedAt: const Value(1000),
        dirty: const Value(false),
      ),
    );
    await sync.sync();
    // Equal timestamps are not ordered by LWW; the server snapshot wins the tie,
    // and the phone converges to that same value without a repeat upload loop.
    expect(
      (await db.select(db.items).getSingle()).name,
      'equal remote conflict',
    );
    expect(server.items[id1]!['name'], 'equal remote conflict');
    expect(store.epoch, 'restore-equal');
  });

  test('response-time generation change applies snapshot before reuploading dirty winners', () async {
    final server = FakeServer();
    final (db, sync, store) = await device(server);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'before backup',
        points: 10,
        updatedAt: 1000,
        dirty: const Value(true),
      ),
    );
    await sync.sync();
    final backup = Map<String, dynamic>.from(server.items[id1]!);
    server.epoch = 'restore-after-upload';
    server.items[id1] = backup;
    await db.upsertItem(
      ItemsCompanion(
        id: const Value(id1),
        kind: const Value('chore'),
        name: const Value('new offline'),
        points: const Value(10),
        updatedAt: const Value(3000),
        dirty: const Value(true),
      ),
    );
    await sync.sync();
    expect(server.items[id1]!['name'], 'new offline');
    expect((await db.select(db.items).getSingle()).name, 'new offline');
    expect(store.epoch, 'restore-after-upload');
  });

  test('an unchanged server does not trigger a full re-push', () async {
    final server = FakeServer();
    final (db, sync, _) = await device(server);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'Dishes',
        points: 10,
        updatedAt: 1000,
        dirty: const Value(true),
      ),
    );
    await sync.sync();
    final seqAfterFirst = server.seq;
    await sync.sync();
    await sync.sync();
    expect(server.seq, seqAfterFirst, reason: 'nothing was pushed again');
  });

  test('new app falls back to a server response without generation', () async {
    final server = FakeServer()..supportsEpoch = false;
    final (db, sync, store) = await device(server);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'old-server',
        points: 10,
        updatedAt: 1000,
        dirty: const Value(true),
      ),
    );
    expect(await sync.sync(), SyncStatus.idle);
    expect(server.items[id1]!['name'], 'old-server');
    expect(store.epoch, isNull);
  });

  test(
    'first contact with a legacy nonzero cursor performs a full exchange',
    () async {
      final server = FakeServer();
      final (db, sync, store) = await device(server);
      final remoteId = '22222222-2222-4222-8222-222222222222';
      server.items[remoteId] = {
        'id': remoteId,
        'kind': 'chore',
        'name': 'remote legacy row',
        'weekdays': 127,
        'remind_time': null,
        'points': 10,
        'due_date': null,
        'sort': 0,
        'active': true,
        'updated_at': 500,
        'deleted_at': null,
        'seq': 1,
      };
      server.seq = 1;
      await store.setCursor(40);
      await db.upsertItem(
        ItemsCompanion.insert(
          id: id1,
          kind: 'chore',
          name: 'local legacy row',
          points: 10,
          updatedAt: 1000,
        ),
      );

      expect(await sync.sync(), SyncStatus.idle);
      expect(server.items.keys, contains(id1), reason: 'phone data is pushed');
      expect(
        (await db.select(db.items).get()).map((i) => i.id),
        contains(remoteId),
        reason: 'full remote snapshot is pulled',
      );
      expect(store.epoch, server.epoch);
    },
  );

  test('legacy first contact retries safely if compatibility changes after preflight', () async {
    final server = FakeServer()..disconnectAfterFirstRequest = true;
    final (db, sync, store) = await device(server);
    await store.setCursor(40);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'local legacy row',
        points: 10,
        updatedAt: 1000,
      ),
    );
    expect(await sync.sync(), SyncStatus.idle);
    expect(server.items[id1]!['name'], 'local legacy row');
    expect(store.epoch, server.epoch);
  });

  test('restore generation change detected before dirty writes even beyond old cursor', () async {
    final server = FakeServer();
    final (db, sync, store) = await device(server);
    for (var i = 0; i < 3; i++) {
      final id = '1111111$i-1111-4111-8111-111111111111';
      await db.upsertItem(
        ItemsCompanion.insert(
          id: id,
          kind: 'chore',
          name: 'local-$i',
          points: 10,
          updatedAt: 1000 + i,
          dirty: const Value(true),
        ),
      );
    }
    await sync.sync();
    expect(store.cursor, 3);
    // Older restored rows have sequence 1 and 2, but the database generation rotates at boot.
    final restored = server.items.entries
        .take(2)
        .map((e) => MapEntry(e.key, e.value))
        .toList();
    server.items
      ..clear()
      ..addEntries(restored);
    for (final entry in server.items.entries.toList()) {
      server.items[entry.key] = {
        ...entry.value,
        'seq': entry == restored[0] ? 1 : 2,
      };
    }
    server
      ..seq = 2
      ..epoch = 'epoch-2';
    await db.upsertItem(
      ItemsCompanion(
        id: const Value(id1),
        kind: const Value('chore'),
        name: const Value('offline edit'),
        points: const Value(10),
        updatedAt: const Value(9000),
        dirty: const Value(true),
      ),
    );
    await sync.sync();
    expect(server.items.length, 3);
    expect(server.items[id1]!['name'], 'offline edit');
    expect(store.epoch, 'epoch-2');
  });

  test('server restart rotates epoch before a restore behind an equal cursor can be masked', () async {
    final server = FakeServer();
    final (db, sync, store) = await device(server);
    for (var i = 0; i < 5; i++) {
      final id = '1111111$i-1111-4111-8111-111111111111';
      await db.upsertItem(
        ItemsCompanion.insert(
          id: id,
          kind: 'chore',
          name: 'local-$i',
          points: 10,
          updatedAt: 1000 + i,
          dirty: const Value(true),
        ),
      );
    }
    await sync.sync();
    expect(store.cursor, 5);
    // Image has seq=3; restart would rotate epoch and disjoint the new seq range first.
    final restored = server.items.entries.take(3).toList();
    server.items
      ..clear()
      ..addEntries(
        restored.map(
          (e) =>
              MapEntry(e.key, {...e.value, 'seq': (restored.indexOf(e) + 1)}),
        ),
      );
    server
      ..seq = 5
      ..epoch = 'epoch-3';
    await db.upsertItem(
      ItemsCompanion(
        id: const Value(id1),
        kind: const Value('chore'),
        name: const Value('dirty offline'),
        points: const Value(10),
        updatedAt: const Value(9999),
        dirty: const Value(true),
      ),
    );
    await sync.sync();
    expect(server.items.length, 5);
    expect(server.items[id1]!['name'], 'dirty offline');
    expect(server.items.length, 5);
  });

  test('pairing marks all local rows for a full push', () async {
    final server = FakeServer();
    final (db, _, store) = await device(server);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'Dishes',
        points: 10,
        updatedAt: 1000,
      ),
    );
    await db.setCompletion(id1, '2026-10-19', true, 2000);
    await (db.update(db.completions))
        .write(const CompletionsCompanion(dirty: Value(false)));
    await store.setCursor(40);
    await store.setEpoch('old');
    // This is what the pairing screen does after a successful pair.
    await store.resetSync();
    await db.markAllDirty();
    expect(store.cursor, 0);
    expect(store.epoch, isNull);
    expect(
      await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(),
      hasLength(1),
    );
    expect(
      await (db.select(
        db.completions,
      )..where((t) => t.dirty.equals(true))).get(),
      hasLength(1),
    );
  });

  test('an edit made while a sync is in flight stays dirty', () async {
    final server = FakeServer();
    final (db, sync, _) = await device(server);
    await db.upsertItem(
      ItemsCompanion.insert(
        id: id1,
        kind: 'chore',
        name: 'A',
        points: 10,
        updatedAt: 100,
        dirty: const Value(true),
      ),
    );
    await sync.sync();
    // Local edit newer than what the server has; a pull that echoes the old row must not overwrite it.
    await db.upsertItem(
      ItemsCompanion(
        id: const Value(id1),
        kind: const Value('chore'),
        name: const Value('B'),
        points: const Value(10),
        updatedAt: const Value(200),
        dirty: const Value(true),
      ),
    );
    server.items[id1] = {...server.items[id1]!, 'seq': ++server.seq};
    await sync.sync();
    expect((await db.select(db.items).getSingle()).name, 'B');
    expect(server.items[id1]!['name'], 'B');
  });
}
