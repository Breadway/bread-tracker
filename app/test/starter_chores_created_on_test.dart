// Regression for TRACK-65 review 10750: addStarterChores passes a precomputed id to saveItem,
// and saveItem used to decide "new" from `id == null`. Those rows were written with created_on
// NULL, so rules.dart scheduled them over all history and could take back a perfect day.
import 'package:drift/drift.dart' show Value;
import 'package:drift/native.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_secure_storage/test/test_flutter_secure_storage_platform.dart';
import 'package:flutter_secure_storage_platform_interface/flutter_secure_storage_platform_interface.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/sync.dart';
import 'package:tracker/rules.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/settings.dart';

Future<ProviderContainer> offlineContainer(AppDb db) async {
  SharedPreferences.setMockInitialValues({});
  FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({});
  final prefs = await SharedPreferences.getInstance();
  final container = ProviderContainer(
    overrides: [
      prefsProvider.overrideWithValue(prefs),
      dbProvider.overrideWithValue(db),
      // Unpaired and offline: the save schedules a sync, and this proves it never reaches out.
      apiFactoryProvider.overrideWithValue((base, token) {
        throw StateError('offline test must not build an API client');
      }),
    ],
  );
  addTearDown(container.dispose);
  return container;
}

void main() {
  testWidgets(
    'adding example chores offline keeps xp, level and earned perfect days',
    (tester) async {
      final db = AppDb(NativeDatabase.memory());
      addTearDown(db.close);
      final now = DateTime.now();
      final today = DateTime.utc(now.year, now.month, now.day);
      int ms(DateTime d) => d.millisecondsSinceEpoch;

      // An account with history: one daily chore, completed on each of the last three days, so
      // every one of those days was scored perfect before the examples are added.
      await db.upsertItem(
        ItemsCompanion(
          id: const Value('c-daily'),
          kind: const Value('chore'),
          name: const Value('Wash up'),
          weekdays: const Value(127),
          points: const Value(10),
          updatedAt: Value(ms(today)),
          createdOn: Value(fmtDay(today.subtract(const Duration(days: 10)))),
          dirty: const Value(false),
        ),
      );
      for (var back = 3; back >= 1; back--) {
        final day = today.subtract(Duration(days: back));
        await db.setCompletion('c-daily', fmtDay(day), true, ms(day));
      }
      final earned = computeStats(
        toRItems(await db.select(db.items).get()),
        toRCompletions(await db.select(db.completions).get()),
        today,
      );
      expect(earned.perfectDays, hasLength(3));
      expect(earned.xp, greaterThan(0));

      final container = await offlineContainer(db);
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: container,
          child: const MaterialApp(home: Scaffold(body: SettingsScreen())),
        ),
      );
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.text('Add example chores'));
      await tester.tap(find.text('Add example chores'));
      await tester.pumpAndSettle();
      // The save schedules a debounced sync. Let it fire so this really is the offline path.
      await tester.pump(const Duration(seconds: 3));
      await tester.pumpAndSettle();

      expect(container.read(syncStatusProvider), SyncStatus.unpaired);

      final after = computeStats(
        toRItems(await db.select(db.items).get()),
        toRCompletions(await db.select(db.completions).get()),
        today,
      );
      expect(after.xp, greaterThanOrEqualTo(earned.xp));
      expect(after.level, greaterThanOrEqualTo(earned.level));
      expect(after.perfectDays, earned.perfectDays);

      final added = (await db.select(db.items).get())
          .where((r) => r.id != 'c-daily')
          .toList();
      expect(added, hasLength(starterChores.length));
      for (final row in added) {
        expect(
          row.createdOn,
          fmtDay(now),
          reason: '${row.name} must be created today, not NULL',
        );
      }
    },
  );

  testWidgets(
    'a precomputed id still gets a creation date and an edit keeps it',
    (tester) async {
      final db = AppDb(NativeDatabase.memory());
      addTearDown(db.close);
      final container = await offlineContainer(db);
      final ctl = container.read(controllerProvider);

      expect(
        await ctl.saveItem(
          id: 'fixed-id',
          kind: 'chore',
          name: 'Fixed',
          creating: true,
        ),
        isTrue,
      );
      final created = (await db.select(db.items).getSingle()).createdOn;
      expect(created, fmtDay(DateTime.now()));

      await ctl.saveItem(
        id: 'fixed-id',
        kind: 'chore',
        name: 'Renamed',
        creating: false,
      );
      final edited = await db.select(db.items).getSingle();
      expect(edited.name, 'Renamed');
      expect(
        edited.createdOn,
        created,
        reason: 'an edit must not move created_on',
      );

      // Creating over an existing row (for example a sync landed the same starter id first) is
      // an insert that does nothing: the stored row and its creation date stay untouched.
      expect(
        await ctl.saveItem(
          id: 'fixed-id',
          kind: 'chore',
          name: 'Different',
          creating: true,
        ),
        isFalse,
      );
      final unchanged = await db.select(db.items).getSingle();
      expect(unchanged.name, 'Renamed');
      expect(unchanged.createdOn, created);

      // Flush the debounced sync timers this test scheduled.
      await tester.pump(const Duration(seconds: 3));
    },
  );
}
