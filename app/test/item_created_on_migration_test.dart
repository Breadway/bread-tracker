import 'dart:io';

import 'package:drift/native.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:sqlite3/sqlite3.dart';
import 'package:tracker/data/database.dart';

/// TRACK-65: schemaVersion 1 installs must gain `created_on` through a real onUpgrade step.
///
/// `createAll` only covers a fresh install. An existing phone already has the version 1 tables,
/// so without the migration opening the database would fail (drift does not silently add
/// columns). The existing row must keep a null creation date: a legacy item has no lower bound,
/// so upgrading changes no XP, level or perfect day.
void main() {
  test('a schemaVersion 1 database upgrades in place', () async {
    final dir = await Directory.systemTemp.createTemp('track65-migration-');
    final file = File('${dir.path}/tracker.sqlite');
    final raw = sqlite3.open(file.path);
    raw.execute('''
      CREATE TABLE items (
        id TEXT NOT NULL PRIMARY KEY,
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        weekdays INTEGER NOT NULL DEFAULT 127,
        remind_time TEXT NULL,
        points INTEGER NOT NULL,
        due_date TEXT NULL,
        sort INTEGER NOT NULL DEFAULT 0,
        active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
        updated_at INTEGER NOT NULL,
        deleted_at INTEGER NULL,
        seq INTEGER NOT NULL DEFAULT 0,
        dirty INTEGER NOT NULL DEFAULT 0 CHECK (dirty IN (0, 1))
      );
    ''');
    raw.execute('''
      CREATE TABLE completions (
        item_id TEXT NOT NULL,
        date TEXT NOT NULL,
        completed_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        deleted_at INTEGER NULL,
        seq INTEGER NOT NULL DEFAULT 0,
        dirty INTEGER NOT NULL DEFAULT 0 CHECK (dirty IN (0, 1)),
        PRIMARY KEY (item_id, date)
      );
    ''');
    raw.execute('''
      INSERT INTO items (id, kind, name, weekdays, remind_time, points, due_date,
                         sort, active, updated_at, deleted_at, seq, dirty)
      VALUES ('legacy-item', 'chore', 'Dishes', 127, NULL, 10, NULL,
              0, 1, 1000, NULL, 4, 0);
    ''');
    raw.userVersion = 1;
    raw.close();

    final db = AppDb(NativeDatabase(file));
    final row = (await db.select(db.items).get()).single;
    expect(row.name, 'Dishes', reason: 'the existing row survived the upgrade');
    expect(row.createdOn, isNull, reason: 'a legacy item keeps no lower bound');

    final columns = await db
        .customSelect(
          "SELECT name FROM pragma_table_info('items') WHERE name = 'created_on'",
        )
        .get();
    expect(columns, hasLength(1), reason: 'the column was added by onUpgrade');
    await db.close();
    await dir.delete(recursive: true);
  });
}
