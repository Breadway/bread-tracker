import 'dart:convert';

import 'package:drift/drift.dart';

import 'api.dart';
import 'database.dart';
import 'store.dart';

enum SyncStatus { idle, syncing, offline, unpaired }

class _RestoreBatch {
  const _RestoreBatch(this.items, this.completions);
  final List<ItemRow> items;
  final List<CompletionRow> completions;
}

class SyncEngine {
  SyncEngine(
    this.db,
    this.store, {
    Api Function(String base, String? token)? apiFactory,
  }) : _apiFactory = apiFactory ?? ((b, t) => Api(b, token: t));
  final AppDb db;
  final Store store;
  final Api Function(String, String?) _apiFactory;
  bool _running = false;
  bool _again = false;

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
    final api = _apiFactory(store.serverUrl, token);

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
      return SyncStatus.unpaired;
    } on ApiException {
      return SyncStatus.offline;
    }
    final serverEpoch = preflight['epoch'] as String?;
    if (serverEpoch != null &&
        requestEpoch != null &&
        serverEpoch != requestEpoch) {
      // Keep the two-part exchange consistent with the protocol: commit the full
      // snapshot first, retain dirty local winners, then retry uploads under the new
      // generation. Never clear sync metadata before the snapshot is committed.
      return _reuploadAfterRestore(preflight, serverEpoch);
    }
    if (serverEpoch != null && requestEpoch == null) {
      // An upgraded install may already have a non-zero legacy cursor but has never
      // recorded the generation it belongs to. Reconcile it as a full exchange once:
      // this recovers phone rows after a pre-upgrade restore and pulls the whole server
      // snapshot before accepting the new generation as the cursor's owner.
      if (store.cursor > 0) {
        await db.transaction(() async {
          for (final row
              in (preflight['items'] as List).cast<Map<String, dynamic>>()) {
            await db.upsertItemIfNewer(_itemCompanion(row));
          }
          for (final row
              in (preflight['completions'] as List)
                  .cast<Map<String, dynamic>>()) {
            await db.upsertCompletionIfNewer(_compCompanion(row));
          }
          await db.markAllDirty();
        });
        await store.setCursor(0);
      }
    }

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
        epoch: store.epoch,
      );
    } on AuthLost {
      return SyncStatus.unpaired;
    } on ApiException {
      return SyncStatus.offline;
    }

    final epoch = res['epoch'] as String?;
    if (epoch != null && epoch != store.epoch) {
      return _reuploadAfterRestore(res, epoch);
    }
    if (epoch != null && requestEpoch == null) {
      await store.setEpoch(epoch);
    }
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

  Future<SyncStatus> _reuploadAfterRestore(
    Map<String, dynamic> snapshot,
    String epoch,
  ) async {
    final winners = await db.transaction(() async {
      for (final row
          in (snapshot['items'] as List).cast<Map<String, dynamic>>()) {
        await db.upsertItemIfNewer(_itemCompanion(row));
      }
      for (final row
          in (snapshot['completions'] as List).cast<Map<String, dynamic>>()) {
        await db.upsertCompletionIfNewer(_compCompanion(row));
      }
      await db.markAllDirty();
      await store.setCursor(0);
      await store.setEpoch(epoch);
      return (
        await (db.select(db.items)..where((r) => r.dirty.equals(true))).get(),
        await (db.select(
          db.completions,
        )..where((r) => r.dirty.equals(true))).get(),
      );
    });
    return _sendRestoreWinners(winners.$1, winners.$2, epoch);
  }

  Future<SyncStatus> _sendRestoreWinners(
    List<ItemRow> items,
    List<CompletionRow> completions,
    String epoch,
  ) async {
    final token = await store.token;
    if (token == null) return SyncStatus.unpaired;
    final api = _apiFactory(store.serverUrl, token);
    var cursor = 0;
    var sentBatch = false;
    for (final batch in _restoreBatches(items, completions)) {
      sentBatch = true;
      final Map<String, dynamic> response;
      try {
        response = await api.sync(
          cursor,
          [for (final row in batch.items) _itemJson(row)],
          [for (final row in batch.completions) _compJson(row)],
          epoch: epoch,
        );
      } on AuthLost {
        return SyncStatus.unpaired;
      } on ApiException {
        return SyncStatus.offline;
      }
      final responseEpoch = response['epoch'] as String?;
      if (responseEpoch != null && responseEpoch != epoch) {
        return SyncStatus.offline;
      }
      await db.transaction(() async {
        for (final row in batch.items) {
          await (db.update(db.items)..where(
                (r) => r.id.equals(row.id) & r.updatedAt.equals(row.updatedAt),
              ))
              .write(const ItemsCompanion(dirty: Value(false)));
        }
        for (final row in batch.completions) {
          await (db.update(db.completions)..where(
                (r) =>
                    r.itemId.equals(row.itemId) &
                    r.date.equals(row.date) &
                    r.updatedAt.equals(row.updatedAt),
              ))
              .write(const CompletionsCompanion(dirty: Value(false)));
        }
        for (final row
            in (response['items'] as List).cast<Map<String, dynamic>>()) {
          final local = await (db.select(
            db.items,
          )..where((r) => r.id.equals(row['id']))).getSingleOrNull();
          if (local == null || local.updatedAt <= (row['updated_at'] as int)) {
            await db.upsertItem(_itemCompanion(row));
          }
        }
        for (final row
            in (response['completions'] as List).cast<Map<String, dynamic>>()) {
          final local =
              await (db.select(db.completions)..where(
                    (r) =>
                        r.itemId.equals(row['item_id']) &
                        r.date.equals(row['date']),
                  ))
                  .getSingleOrNull();
          if (local == null || local.updatedAt <= (row['updated_at'] as int)) {
            await db
                .into(db.completions)
                .insertOnConflictUpdate(_compCompanion(row));
          }
        }
      });
      cursor = response['cursor'] as int;
      await store.setCursor(cursor);
    }
    if (!sentBatch) {
      await store.setCursor(0);
    }
    return SyncStatus.idle;
  }

  Iterable<_RestoreBatch> _restoreBatches(
    List<ItemRow> items,
    List<CompletionRow> completions,
  ) sync* {
    var batchItems = <ItemRow>[];
    var batchCompletions = <CompletionRow>[];
    for (final row in items) {
      final candidate = [...batchItems, row];
      if (batchItems.isNotEmpty &&
          (candidate.length + batchCompletions.length > 5000 ||
              utf8
                      .encode(
                        jsonEncode({
                          'since': 0,
                          'epoch': store.epoch,
                          'items': [
                            for (final value in candidate) _itemJson(value),
                          ],
                          'completions': [
                            for (final value in batchCompletions)
                              _compJson(value),
                          ],
                        }),
                      )
                      .length >
                  1024 * 1024)) {
        yield _RestoreBatch(batchItems, batchCompletions);
        batchItems = [];
        batchCompletions = [];
      }
      batchItems.add(row);
    }
    for (final row in completions) {
      final candidate = [...batchCompletions, row];
      if (batchItems.length + batchCompletions.length >= 5000 ||
          (batchItems.isNotEmpty || batchCompletions.isNotEmpty) &&
              utf8
                      .encode(
                        jsonEncode({
                          'since': 0,
                          'epoch': store.epoch,
                          'items': [
                            for (final value in batchItems) _itemJson(value),
                          ],
                          'completions': [
                            for (final value in candidate) _compJson(value),
                          ],
                        }),
                      )
                      .length >
                  1024 * 1024) {
        yield _RestoreBatch(batchItems, batchCompletions);
        batchItems = [];
        batchCompletions = [];
      }
      batchCompletions.add(row);
    }
    if (batchItems.isNotEmpty || batchCompletions.isNotEmpty) {
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
