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
  AppDb([QueryExecutor? e])
    : super(
        e ??
            driftDatabase(
              name: 'tracker',
              // The notification action runs in a second isolate on the same file, so a
              // writer must wait for the other instead of failing with "database is locked".
              native: DriftNativeOptions(
                setup: (db) => db.execute('PRAGMA busy_timeout = 5000'),
              ),
            ),
      );

  /// Re-run watch queries. Another isolate (the notification action) writes to the same
  /// file without telling this one, so call this when the app comes back to the foreground.
  void refreshStreams() => markTablesUpdated([items, completions]);

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

  Future<void> softDeleteItem(String id, int now) =>
      (update(items)..where((t) => t.id.equals(id))).write(
        ItemsCompanion(
          deletedAt: Value(now),
          updatedAt: Value(now),
          dirty: const Value(true),
        ),
      );
}
