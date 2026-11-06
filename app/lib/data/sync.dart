import 'dart:convert';

import 'package:drift/drift.dart';

import 'api.dart';
import 'database.dart';
import 'store.dart';

enum SyncStatus { idle, syncing, offline, unpaired, rejected }

class _RestoreBatch {
  const _RestoreBatch(this.items, this.completions);
  final List<ItemRow> items;
  final List<CompletionRow> completions;
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

    final requestEpoch = store.epoch;
    Map<String, dynamic> preflight;
    try {
      preflight = await api.sync(
        store.cursor,
        const [],
        const [],
        epoch: requestEpoch,
      );
    } on AuthLost {
      return _authLost(token, storeGeneration, generation, server);
    } on ApiException catch (error) {
      return _apiFailure(error);
    }
    if (!_isCurrent(generation, storeGeneration, server)) {
      return SyncStatus.unpaired;
    }
    final serverEpoch = preflight['epoch'] as String?;

    if (serverEpoch != null &&
        requestEpoch != null &&
        serverEpoch != requestEpoch) {
      // A restored or rebuilt server. Apply the complete snapshot with plain LWW first
      // (dirty is not a freshness indicator: a synced phone row can be newer than a
      // restored backup), then offer every local row again under the new generation.
      // The new epoch is committed only after the last upload succeeds.
      return _reuploadAfterRestore(
        preflight,
        serverEpoch,
        token: token,
        generation: generation,
        storeGeneration: storeGeneration,
        server: server,
      );
    }
    if (serverEpoch != null && requestEpoch == null && store.cursor > 0) {
      // An upgraded install has a cursor from a server it never recorded an epoch for.
      // Reconcile it once as a full exchange: pull the whole snapshot before accepting
      // the new generation as the owner of the cursor.
      if (!await _applySnapshot(preflight, storeGeneration, generation, server)) {
        return SyncStatus.unpaired;
      }
      // This install never recorded a generation, so its clean rows may never have been
      // offered to this server. Reoffer all of them, as re-pairing does.
      await db.markAllDirty();
      if (!await store.resetSyncIfGeneration(
        storeGeneration,
        expectedServer: server,
      )) {
        return SyncStatus.unpaired;
      }
    }

    return _pushDirtyBatches(
      epoch: serverEpoch,
      startCursor: store.cursor,
      token: token,
      generation: generation,
      storeGeneration: storeGeneration,
      server: server,
    );
  }

  Future<SyncStatus> _authLost(
    String token,
    int storeGeneration,
    int generation,
    String server,
  ) async {
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
    }
    return SyncStatus.unpaired;
  }

  bool _isCurrent(int generation, int storeGeneration, String server) =>
      generation == _generation &&
      !_invalidated &&
      store.authGeneration == storeGeneration &&
      store.serverUrl == server;

  /// A reachable 4xx answer is a policy rejection, not a network outage: surface the
  /// server's message instead of retrying forever. Everything else stays offline.
  SyncStatus _apiFailure(ApiException error) {
    final code = error.statusCode;
    if (code != null && code >= 400 && code < 500) {
      rejectionMessage = error.message;
      return SyncStatus.rejected;
    }
    return SyncStatus.offline;
  }

  /// Apply a remote snapshot row by row, keeping whichever copy has the newer
  /// `updated_at` regardless of its dirty flag.
  Future<bool> _applySnapshot(
    Map<String, dynamic> snapshot,
    int storeGeneration,
    int generation,
    String server,
  ) async {
    if (!_isCurrent(generation, storeGeneration, server)) return false;
    await db.transaction(() async {
      for (final row
          in (snapshot['items'] as List).cast<Map<String, dynamic>>()) {
        await db.upsertItemIfNewer(_itemCompanion(row));
      }
      for (final row
          in (snapshot['completions'] as List).cast<Map<String, dynamic>>()) {
        await db.upsertCompletionIfNewer(_compCompanion(row));
      }
    });
    return _isCurrent(generation, storeGeneration, server);
  }

  Future<SyncStatus> _reuploadAfterRestore(
    Map<String, dynamic> snapshot,
    String epoch, {
    required String token,
    required int generation,
    required int storeGeneration,
    required String server,
  }) async {
    if (!await _applySnapshot(snapshot, storeGeneration, generation, server)) {
      return SyncStatus.unpaired;
    }
    if (!_isCurrent(generation, storeGeneration, server)) {
      return SyncStatus.unpaired;
    }
    await db.markAllDirty();
    if (!await store.resetSyncIfGeneration(
      storeGeneration,
      expectedServer: server,
    )) {
      return SyncStatus.unpaired;
    }
    return _pushDirtyBatches(
      epoch: epoch,
      startCursor: 0,
      token: token,
      generation: generation,
      storeGeneration: storeGeneration,
      server: server,
    );
  }

  /// Send every dirty row in bounded batches and apply each response. The server
  /// enforces the same batch cap as the normal route, so a large history, a re-pair or
  /// a restore retried after a transient failure must never go out as one POST.
  ///
  /// `epoch` is committed only after every batch has succeeded. A failure therefore
  /// leaves the old (or missing) epoch in place, so the next sync repeats preflight
  /// detection and resumes with the rows that are still dirty.
  Future<SyncStatus> _pushDirtyBatches({
    required String? epoch,
    required int startCursor,
    required String token,
    required int generation,
    required int storeGeneration,
    required String server,
  }) async {
    final dirtyItems = await (db.select(
      db.items,
    )..where((t) => t.dirty.equals(true))).get();
    final dirtyComps = await (db.select(
      db.completions,
    )..where((t) => t.dirty.equals(true))).get();
    final api = _apiFactory(server, token);
    var cursor = startCursor;
    String? responseEpoch;

    for (final batch in _exchangeBatches(dirtyItems, dirtyComps, epoch)) {
      final Map<String, dynamic> res;
      try {
        res = await api.sync(
          cursor,
          [for (final row in batch.items) _itemJson(row)],
          [for (final row in batch.completions) _compJson(row)],
          epoch: epoch,
        );
      } on AuthLost {
        return _authLost(token, storeGeneration, generation, server);
      } on ApiException catch (error) {
        return _apiFailure(error);
      }
      if (!_isCurrent(generation, storeGeneration, server)) {
        return SyncStatus.unpaired;
      }
      responseEpoch = res['epoch'] as String?;
      if (epoch != null &&
          responseEpoch != null &&
          responseEpoch != epoch) {
        // A second restore landed during this exchange. Restart from preflight.
        return SyncStatus.offline;
      }
      await db.transaction(() async {
        // Rows unchanged since we pushed them are now clean.
        for (final i in batch.items) {
          await (db.update(db.items)..where(
                (t) => t.id.equals(i.id) & t.updatedAt.equals(i.updatedAt),
              ))
              .write(const ItemsCompanion(dirty: Value(false)));
        }
        for (final c in batch.completions) {
          await (db.update(db.completions)..where(
                (t) =>
                    t.itemId.equals(c.itemId) &
                    t.date.equals(c.date) &
                    t.updatedAt.equals(c.updatedAt),
              ))
              .write(const CompletionsCompanion(dirty: Value(false)));
        }
        // LWW regardless of dirty: an already-synced local row can be newer than a
        // restored backup, and a newer remote row must win over a clean local one.
        for (final j in (res['items'] as List).cast<Map<String, dynamic>>()) {
          final local = await (db.select(
            db.items,
          )..where((t) => t.id.equals(j['id']))).getSingleOrNull();
          if (local != null && local.updatedAt > (j['updated_at'] as int)) {
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
          if (local != null && local.updatedAt > (j['updated_at'] as int)) {
            continue;
          }
          await db
              .into(db.completions)
              .insertOnConflictUpdate(_compCompanion(j));
        }
      });
      cursor = res['cursor'] as int;
      if (!await store.setCursorIfGeneration(
        cursor,
        storeGeneration,
        expectedServer: server,
      )) {
        return SyncStatus.unpaired;
      }
      if (!_isCurrent(generation, storeGeneration, server)) {
        return SyncStatus.unpaired;
      }
    }

    // Commit the generation only after the whole exchange is durable on the server.
    final committedEpoch = epoch ?? responseEpoch;
    if (committedEpoch != null && committedEpoch != store.epoch) {
      if (!await store.setEpochIfGeneration(
        committedEpoch,
        storeGeneration,
        expectedServer: server,
      )) {
        return SyncStatus.unpaired;
      }
    }
    if (!_isCurrent(generation, storeGeneration, server)) {
      return SyncStatus.unpaired;
    }
    return SyncStatus.idle;
  }

  /// Split dirty rows into requests the server will accept. One empty batch is yielded
  /// when there is nothing to push so the pull side still runs.
  Iterable<_RestoreBatch> _exchangeBatches(
    List<ItemRow> items,
    List<CompletionRow> completions,
    String? epoch,
  ) sync* {
    var batchItems = <ItemRow>[];
    var batchCompletions = <CompletionRow>[];
    var yielded = false;

    bool overLimit(ItemRow? nextItem, CompletionRow? nextCompletion) {
      if (batchItems.length + batchCompletions.length + 1 > 5000) return true;
      final size = utf8
          .encode(
            jsonEncode({
              'since': 0,
              'epoch': epoch,
              'items': [
                for (final value in batchItems) _itemJson(value),
                if (nextItem != null) _itemJson(nextItem),
              ],
              'completions': [
                for (final value in batchCompletions) _compJson(value),
                if (nextCompletion != null) _compJson(nextCompletion),
              ],
            }),
          )
          .length;
      return size > 1024 * 1024;
    }

    for (final row in items) {
      if ((batchItems.isNotEmpty || batchCompletions.isNotEmpty) &&
          overLimit(row, null)) {
        yield _RestoreBatch(batchItems, batchCompletions);
        yielded = true;
        batchItems = [];
        batchCompletions = [];
      }
      batchItems.add(row);
    }
    for (final row in completions) {
      if ((batchItems.isNotEmpty || batchCompletions.isNotEmpty) &&
          overLimit(null, row)) {
        yield _RestoreBatch(batchItems, batchCompletions);
        yielded = true;
        batchItems = [];
        batchCompletions = [];
      }
      batchCompletions.add(row);
    }
    if (batchItems.isNotEmpty || batchCompletions.isNotEmpty || !yielded) {
      yield _RestoreBatch(batchItems, batchCompletions);
    }
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
