import 'package:drift/drift.dart';

import 'api.dart';
import 'database.dart';
import 'store.dart';

/// Outcome of a sync pass. Policy refusals are distinct from [offline]: the
/// server was reachable and answered, but retrying cannot fix the account
/// state without a user or owner action.
enum SyncStatus {
  idle,
  syncing,
  offline,
  unpaired,
  rejected,
  quotaExhausted,
  accountDisabled,
  rateLimited,
}

extension SyncStatusPolicy on SyncStatus {
  bool get isPolicyRejection =>
      this == SyncStatus.quotaExhausted ||
      this == SyncStatus.accountDisabled ||
      this == SyncStatus.rateLimited ||
      this == SyncStatus.rejected;
}

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
  String? _token;
  String? _server;
  String? rejectionMessage;

  void invalidate() {
    _generation++;
    _invalidated = true;
    _again = false;
  }

  void revalidate() {
    _generation++;
    _invalidated = false;
  }

  /// Close new sync admission while sign-out owns local cleanup.
  void beginSignOut() => invalidate();

  /// Push dirty rows, pull everything newer than the cursor. Safe to call often;
  /// overlapping calls collapse into one follow-up pass.
  Future<SyncStatus> sync() async {
    if (_invalidated) return SyncStatus.unpaired;
    if (_running) {
      if (!_invalidated) _again = true;
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
      if (_invalidated) _again = false;
    }
  }

  Future<SyncStatus> _once() async {
    rejectionMessage = null;
    if (_invalidated) return SyncStatus.unpaired;
    final storeGeneration = store.authGeneration;
    final token = await store.readTokenForGeneration(storeGeneration);
    if (_invalidated || store.authGeneration != storeGeneration) {
      return SyncStatus.unpaired;
    }
    if (token == null) return SyncStatus.unpaired;
    if (_token != null && (_token != token || _server != store.serverUrl)) {
      _generation++;
    }
    _token = token;
    final generation = _generation;
    final server = store.serverUrl;
    _server = server;
    final api = _apiFactory(server, token);

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
      if (generation == _generation &&
          !_invalidated &&
          await store.clearTokenIfCurrentForGeneration(
            token,
            storeGeneration,
            expectedServer: server,
          )) {
        _invalidated = true;
        _generation++;
        return SyncStatus.unpaired;
      }
      return SyncStatus.unpaired;
    } on ApiException catch (error) {
      final status = error.statusCode;
      if (status != null && status >= 400 && status < 500) {
        // The token is valid enough to be answered, so keep it and the dirty
        // rows. The status tells the UI whether waiting, freeing quota or an
        // owner action is the only way forward.
        rejectionMessage = error.message;
        return switch (status) {
          403 => SyncStatus.accountDisabled,
          422 => SyncStatus.quotaExhausted,
          429 => SyncStatus.rateLimited,
          _ => SyncStatus.rejected,
        };
      }
      return SyncStatus.offline;
    }

    if (generation != _generation ||
        _invalidated ||
        store.authGeneration != storeGeneration ||
        store.serverUrl != server) {
      return SyncStatus.unpaired;
    }
    final cursor = res['cursor'] as int;
    await db.transaction(() async {
      // Re-check inside the same database write transaction used for applying the
      // response. Sign-out performs its local wipe in a database transaction too.
      if (generation != _generation ||
          _invalidated ||
          store.authGeneration != storeGeneration ||
          store.serverUrl != server) {
        return;
      }
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
    if (generation != _generation ||
        _invalidated ||
        store.authGeneration != storeGeneration ||
        store.serverUrl != server) {
      return SyncStatus.unpaired;
    }
    await store.setCursorIfGeneration(
      cursor,
      storeGeneration,
      expectedServer: server,
    );
    if (generation != _generation ||
        _invalidated ||
        store.authGeneration != storeGeneration) {
      return SyncStatus.unpaired;
    }
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
    'created_on': i.createdOn,
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
    // Additive: an old server omits the field, and a stored value must survive that pull.
    createdOn: j['created_on'] == null
        ? const Value.absent()
        : Value(j['created_on'] as String),
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
