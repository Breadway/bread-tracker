import 'package:drift/drift.dart';
import 'package:drift_flutter/drift_flutter.dart';

part 'database.g.dart';

@DataClassName('ItemRow')
class Items extends Table {
  TextColumn get id => text()();
  TextColumn get kind => text()();
  TextColumn get name => text()();
  IntColumn get weekdays => integer().withDefault(const Constant(127))();
  TextColumn get remindTime => text().nullable()();
  IntColumn get points => integer()();
  TextColumn get dueDate => text().nullable()();
  IntColumn get sort => integer().withDefault(const Constant(0))();
  BoolColumn get active => boolean().withDefault(const Constant(true))();
  IntColumn get updatedAt => integer()();
  IntColumn get deletedAt => integer().nullable()();
  IntColumn get seq => integer().withDefault(const Constant(0))();
  BoolColumn get dirty => boolean().withDefault(const Constant(false))();

  @override
  Set<Column> get primaryKey => {id};
}

@DataClassName('CompletionRow')
class Completions extends Table {
  TextColumn get itemId => text()();
  TextColumn get date => text()();
  IntColumn get completedAt => integer()();
  IntColumn get updatedAt => integer()();
  IntColumn get deletedAt => integer().nullable()();
  IntColumn get seq => integer().withDefault(const Constant(0))();
  BoolColumn get dirty => boolean().withDefault(const Constant(false))();

  @override
  Set<Column> get primaryKey => {itemId, date};
}

@DriftDatabase(tables: [Items, Completions])
class AppDb extends _$AppDb {
  AppDb([QueryExecutor? e]) : super(e ?? driftDatabase(name: 'tracker'));

  @override
  int get schemaVersion => 1;

  Stream<List<ItemRow>> watchItems() =>
      (select(items)
            ..where((t) => t.deletedAt.isNull())
            ..orderBy([
              (t) => OrderingTerm.asc(t.sort),
              (t) => OrderingTerm.asc(t.remindTime),
              (t) => OrderingTerm.asc(t.name),
            ]))
          .watch();

  Stream<List<CompletionRow>> watchCompletions() =>
      (select(completions)..where((t) => t.deletedAt.isNull())).watch();

  Future<void> upsertItem(ItemsCompanion c) =>
      into(items).insertOnConflictUpdate(c);

  Future<void> setCompletion(
    String itemId,
    String date,
    bool done,
    int now,
  ) async {
    final existing =
        await (select(completions)
              ..where((t) => t.itemId.equals(itemId) & t.date.equals(date)))
            .getSingleOrNull();
    if (existing == null && !done) return;
    await into(completions).insertOnConflictUpdate(
      CompletionsCompanion(
        itemId: Value(itemId),
        date: Value(date),
        completedAt: Value(done ? now : (existing?.completedAt ?? now)),
        updatedAt: Value(now),
        deletedAt: Value(done ? null : now),
        seq: Value(existing?.seq ?? 0),
        dirty: const Value(true),
      ),
    );
  }

  /// Mark every local row as unsynced so the next sync pushes all of them. Safe because the
  /// server keeps whichever copy has the newer `updated_at`.
  Future<void> markAllDirty() async {
    await (update(items)).write(const ItemsCompanion(dirty: Value(true)));
    await (update(completions))
        .write(const CompletionsCompanion(dirty: Value(true)));
  }

  /// Apply a remote snapshot row only when it is newer than the local row. Dirty status
  /// is not a freshness indicator: already-synced phone data can be newer than a backup.
  Future<void> upsertItemIfNewer(ItemsCompanion incoming) async {
    final id = incoming.id.value;
    final local = await (select(
      items,
    )..where((row) => row.id.equals(id))).getSingleOrNull();
    if (local != null && local.updatedAt > incoming.updatedAt.value) {
      return;
    }
    await upsertItem(incoming);
  }

  Future<void> upsertCompletionIfNewer(CompletionsCompanion incoming) async {
    final itemId = incoming.itemId.value;
    final date = incoming.date.value;
    final local =
        await (select(completions)..where(
              (row) => row.itemId.equals(itemId) & row.date.equals(date),
            ))
            .getSingleOrNull();
    if (local != null && local.updatedAt > incoming.updatedAt.value) {
      return;
    }
    await into(completions).insertOnConflictUpdate(incoming);
  }

  /// Whether there is anything on the phone to lose (a tombstone counts: it is unsynced state).
  Future<bool> hasLocalData() async {
    final i = await (select(items)..limit(1)).get();
    if (i.isNotEmpty) return true;
    return (await (select(completions)..limit(1)).get()).isNotEmpty;
  }

  /// Delete every local item and completion (used when signing out and wiping the phone).
  Future<void> clearAll() => transaction(() async {
    await delete(completions).go();
    await delete(items).go();
  });

  Future<void> softDeleteItem(String id, int now) =>
      (update(items)..where((t) => t.id.equals(id))).write(
        ItemsCompanion(
          deletedAt: Value(now),
          updatedAt: Value(now),
          dirty: const Value(true),
        ),
      );
}
