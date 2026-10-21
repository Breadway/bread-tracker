import 'package:drift/drift.dart';

import 'api.dart';
import 'database.dart';
import 'store.dart';

enum SyncStatus { idle, syncing, offline, unpaired }

class SyncEngine {
  SyncEngine(
    this.db,
    this.store, {
    Api Function(String base, String token)? apiFactory,
  }) : _apiFactory = apiFactory ?? ((b, t) => Api(b, token: t));
  final AppDb db;
  final Store store;
  final Api Function(String, String) _apiFactory;
  bool _running = false;
  bool _again = false;
  int _generation = 0;
  bool _invalidated = false;

  void invalidate() {
    _generation++;
    _invalidated = true;
  }

  void revalidate() {
    _generation++;
    _invalidated = false;
  }

  /// Push dirty rows, pull everything newer than the cursor. Safe to call often;
  /// overlapping calls collapse into one follow-up pass.
  Future<SyncStatus> sync() async {
    if (_running) {
      _again = true;
      return SyncStatus.syncing;
    }
    _running = true;
    try {
      SyncStatus status;
      do {
        _again = false;
        status = await _once();
      } while (_again && status == SyncStatus.idle);
      return status;
    } finally {
      _running = false;
    }
  }

  Future<SyncStatus> _once() async {
    final token = await store.token;
    if (token == null) return SyncStatus.unpaired;
    if (_invalidated) revalidate();
    final generation = _generation;
    final api = _apiFactory(store.serverUrl, token);

    final dirtyItems = await (db.select(
      db.items,
    )..where((t) => t.dirty.equals(true))).get();
    final dirtyComps = await (db.select(
      db.completions,
    )..where((t) => t.dirty.equals(true))).get();

    final Map<String, dynamic> res;
    try {
      res = await api.sync(
        store.cursor,
        [for (final i in dirtyItems) _itemJson(i)],
        [for (final c in dirtyComps) _compJson(c)],
      );
    } on AuthLost {
      // The server no longer knows this token (device revoked). Forget it instead of
      // retrying a dead credential forever; local data is kept for the next pairing.
      await store.clearToken();
      _invalidated = true;
      return SyncStatus.unpaired;
    } on ApiException {
      return SyncStatus.offline;
    }

    if (generation != _generation || _invalidated) return SyncStatus.unpaired;

    await db.transaction(() async {
      // Rows unchanged since we pushed them are now clean.
      for (final i in dirtyItems) {
        await (db.update(db.items)..where(
              (t) => t.id.equals(i.id) & t.updatedAt.equals(i.updatedAt),
            ))
            .write(const ItemsCompanion(dirty: Value(false)));
      }
      for (final c in dirtyComps) {
        await (db.update(db.completions)..where(
              (t) =>
                  t.itemId.equals(c.itemId) &
                  t.date.equals(c.date) &
                  t.updatedAt.equals(c.updatedAt),
            ))
            .write(const CompletionsCompanion(dirty: Value(false)));
      }
      for (final j in (res['items'] as List).cast<Map<String, dynamic>>()) {
        final local = await (db.select(
          db.items,
        )..where((t) => t.id.equals(j['id']))).getSingleOrNull();
        if (local != null &&
            local.dirty &&
            local.updatedAt > (j['updated_at'] as int)) {
          continue;
        }
        await db.upsertItem(_itemCompanion(j));
      }
      for (final j
          in (res['completions'] as List).cast<Map<String, dynamic>>()) {
        final local =
            await (db.select(db.completions)..where(
                  (t) =>
                      t.itemId.equals(j['item_id']) & t.date.equals(j['date']),
                ))
                .getSingleOrNull();
        if (local != null &&
            local.dirty &&
            local.updatedAt > (j['updated_at'] as int)) {
          continue;
        }
        await db.into(db.completions).insertOnConflictUpdate(_compCompanion(j));
      }
    });
    await store.setCursor(res['cursor'] as int);
    return SyncStatus.idle;
  }

  Map<String, dynamic> _itemJson(ItemRow i) => {
    'id': i.id,
    'kind': i.kind,
    'name': i.name,
    'weekdays': i.weekdays,
    'remind_time': i.remindTime,
    'points': i.points,
    'due_date': i.dueDate,
    'sort': i.sort,
    'active': i.active,
    'updated_at': i.updatedAt,
    'deleted_at': i.deletedAt,
  };

  Map<String, dynamic> _compJson(CompletionRow c) => {
    'item_id': c.itemId,
    'date': c.date,
    'completed_at': c.completedAt,
    'updated_at': c.updatedAt,
    'deleted_at': c.deletedAt,
  };

  ItemsCompanion _itemCompanion(Map<String, dynamic> j) => ItemsCompanion(
    id: Value(j['id']),
    kind: Value(j['kind']),
    name: Value(j['name']),
    weekdays: Value(j['weekdays']),
    remindTime: Value(j['remind_time']),
    points: Value(j['points']),
    dueDate: Value(j['due_date']),
    sort: Value(j['sort']),
    active: Value(j['active']),
    updatedAt: Value(j['updated_at']),
    deletedAt: Value(j['deleted_at']),
    seq: Value(j['seq'] ?? 0),
    dirty: const Value(false),
  );

  CompletionsCompanion _compCompanion(Map<String, dynamic> j) =>
      CompletionsCompanion(
        itemId: Value(j['item_id']),
        date: Value(j['date']),
        completedAt: Value(j['completed_at']),
        updatedAt: Value(j['updated_at']),
        deletedAt: Value(j['deleted_at']),
        seq: Value(j['seq'] ?? 0),
        dirty: const Value(false),
      );
}
