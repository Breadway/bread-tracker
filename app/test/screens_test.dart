import 'dart:io';

import 'package:drift/drift.dart' show Value, driftRuntimeOptions;
import 'package:drift/native.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/app.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/rules.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/pair.dart';

/// Renders the real screens headlessly with the real font. Run with --update-goldens to
/// regenerate the PNGs in test/goldens for a visual check.
class NoTokenStore extends Store {
  NoTokenStore(super.prefs);
  @override
  Future<String?> get token async => null;
}

/// Screenshots are only written with --update-goldens (they depend on today's date, so they
/// are for eyeballing, not for asserting).
Future<void> shot(String path) async {
  if (autoUpdateGoldenFiles) {
    await expectLater(find.byType(MaterialApp), matchesGoldenFile(path));
  }
}

Future<void> loadFont() async {
  final loader = FontLoader('VarelaRound')
    ..addFont(Future.value(ByteData.view(File('assets/fonts/VarelaRound-Regular.ttf').readAsBytesSync().buffer)));
  await loader.load();
  final icons = File('${Platform.environment['HOME']}/.local/flutter/bin/cache/artifacts/material_fonts/MaterialIcons-Regular.otf');
  if (icons.existsSync()) {
    await (FontLoader('MaterialIcons')..addFont(Future.value(ByteData.view(icons.readAsBytesSync().buffer)))).load();
  }
}

Future<AppDb> seededDb() async {
  final db = AppDb(NativeDatabase.memory());
  final fixedToday = DateTime.now();
  final today = DateTime.utc(fixedToday.year, fixedToday.month, fixedToday.day);
  int ms(DateTime d) => d.millisecondsSinceEpoch;
  Future<void> item(String id, String kind, String name, int mask, String? time, int pts) => db.upsertItem(ItemsCompanion(
        id: Value(id), kind: Value(kind), name: Value(name), weekdays: Value(mask), remindTime: Value(time), points: Value(pts),
        updatedAt: Value(ms(today)), dirty: const Value(false)));
  // every weekday has the daily chore and four habits; a few weekday-specific chores
  await item('c-dinner', 'chore', 'Wash or dry dishes, or cook dinner', 127, '18:30', 10);
  await item('c-recycle', 'chore', 'Recycling', 1, '17:15', 10);
  await item('c-worm', 'chore', 'Worm Food', 4, '16:00', 10);
  await item('c-wash', 'chore', 'Put washing in machine', 16, '15:30', 10);
  await item('c-bath', 'chore', 'Clean bathroom', 16, '17:00', 10);
  await item('c-room', 'chore', 'Room clean', 32, '09:30', 10);
  await item('h-gym', 'habit', 'Gym or workout', 127, null, 15);
  await item('h-read', 'habit', 'Read 20 minutes', 127, null, 15);
  await item('h-shoot', 'habit', 'Shoot 50 free throws', 127, null, 15);
  await item('t-1', 'todo', 'Revise area and volume for the Maths assessment', 0, null, 20);
  await item('t-2', 'todo', 'Book haircut', 0, null, 20);
  for (var d = 6; d >= 1; d--) {
    final day = today.subtract(Duration(days: d));
    final ds = fmtDay(day);
    await db.setCompletion('h-read', ds, true, ms(day));
    if (d % 2 == 0) await db.setCompletion('h-gym', ds, true, ms(day));
    if (d != 3) await db.setCompletion('c-dinner', ds, true, ms(day));
  }
  await db.setCompletion('h-read', fmtDay(today), true, ms(today));
  return db;
}

void main() {
  driftRuntimeOptions.dontWarnAboutMultipleDatabases = true;
  TestWidgetsFlutterBinding.ensureInitialized();
  // connectivity_plus has no host implementation under flutter test.
  TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger
      .setMockMethodCallHandler(const MethodChannel('dev.fluttercommunity.plus/connectivity_status'), (call) async => null);

  testWidgets('today, todos, stats, settings and pairing render', (tester) async {
    await loadFont();
    SharedPreferences.setMockInitialValues({});
    final prefs = await SharedPreferences.getInstance();
    final db = await seededDb();
    tester.view.physicalSize = const Size(1080, 2340);
    tester.view.devicePixelRatio = 2.625;
    addTearDown(tester.view.reset);

    final container = ProviderContainer(overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(NoTokenStore(prefs)),
      dbProvider.overrideWithValue(db),
      pairedProvider.overrideWith((ref) async => true),
    ]);
    addTearDown(container.dispose);

    await tester.pumpWidget(UncontrolledProviderScope(container: container, child: const TrackerApp()));
    await tester.pump(const Duration(milliseconds: 600));
    await tester.pump(const Duration(milliseconds: 600));
    await shot('goldens/today.png');

    // Tick a chore: floating XP and confetti appear, the row turns into its done state.
    final first = find.text('Wash or dry dishes, or cook dinner');
    await tester.tap(first);
    await tester.pump(const Duration(milliseconds: 120));
    await shot('goldens/today_tick.png');
    await tester.pump(const Duration(seconds: 3));

    for (final (i, name) in [(1, 'todos'), (2, 'stats'), (3, 'settings')]) {
      await tester.tap(find.byType(NavigationDestination).at(i));
      await tester.pump(const Duration(milliseconds: 700));
      await shot('goldens/$name.png');
    }
  });

  testWidgets('pairing screen renders', (tester) async {
    await loadFont();
    SharedPreferences.setMockInitialValues({});
    final prefs = await SharedPreferences.getInstance();
    tester.view.physicalSize = const Size(1080, 2340);
    tester.view.devicePixelRatio = 2.625;
    addTearDown(tester.view.reset);
    final container = ProviderContainer(overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(NoTokenStore(prefs)),
      dbProvider.overrideWithValue(AppDb(NativeDatabase.memory())),
      pairedProvider.overrideWith((ref) async => false),
    ]);
    addTearDown(container.dispose);
    await tester.pumpWidget(UncontrolledProviderScope(container: container, child: const TrackerApp()));
    await tester.pump(const Duration(milliseconds: 600));
    await shot('goldens/pair.png');
  });

  testWidgets('dark theme: a perfect day fires the toast and confetti', (tester) async {
    await loadFont();
    SharedPreferences.setMockInitialValues({});
    final prefs = await SharedPreferences.getInstance();
    final db = await seededDb();
    tester.platformDispatcher.platformBrightnessTestValue = Brightness.dark;
    addTearDown(tester.platformDispatcher.clearAllTestValues);
    tester.view.physicalSize = const Size(1080, 2340);
    tester.view.devicePixelRatio = 2.625;
    addTearDown(tester.view.reset);
    final container = ProviderContainer(overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(NoTokenStore(prefs)),
      dbProvider.overrideWithValue(db),
      pairedProvider.overrideWith((ref) async => true),
    ]);
    addTearDown(container.dispose);
    await tester.pumpWidget(UncontrolledProviderScope(container: container, child: const TrackerApp()));
    await tester.pump(const Duration(milliseconds: 600));
    await tester.pump(const Duration(milliseconds: 600));
    await shot('goldens/dark_today.png');

    await tester.tap(find.text('Gym or workout'));
    await tester.pump(const Duration(milliseconds: 200));
    await tester.tap(find.text('Shoot 50 free throws'));
    await tester.pump(const Duration(milliseconds: 200));
    // The seed also has weekday-specific chores (see seededDb), so finish whatever is
    // scheduled today before the last tick; otherwise this only passes on some weekdays.
    const todaysExtras = {
      DateTime.monday: ['Recycling'],
      DateTime.wednesday: ['Worm Food'],
      DateTime.friday: ['Put washing in machine', 'Clean bathroom'],
      DateTime.saturday: ['Room clean'],
    };
    for (final name in todaysExtras[DateTime.now().weekday] ?? const <String>[]) {
      await tester.ensureVisible(find.text(name));
      await tester.tap(find.text(name));
      await tester.pump(const Duration(milliseconds: 200));
    }
    await tester.tap(find.text('Wash or dry dishes, or cook dinner'));
    await tester.pump(const Duration(milliseconds: 100));
    await tester.pump(const Duration(milliseconds: 300));
    await shot('goldens/dark_perfect.png');
    expect(find.textContaining('Perfect day'), findsOneWidget);
    await tester.pump(const Duration(seconds: 4));
  });
}
