import 'package:drift/native.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/pair.dart';
import 'package:tracker/ui/today.dart';

const _owner = ['Worm Food', 'game night', 'Recycling', 'Bring in washing', 'Put washing away', 'Hang out washing'];

Future<ProviderContainer> pump(WidgetTester tester, Widget home, {String? token}) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final db = AppDb(NativeDatabase.memory());
  addTearDown(db.close);
  final c = ProviderContainer(overrides: [
    prefsProvider.overrideWithValue(prefs),
    dbProvider.overrideWithValue(db),
  ]);
  addTearDown(c.dispose);
  tester.view.physicalSize = const Size(1080, 2400);
  tester.view.devicePixelRatio = 2.625;
  addTearDown(tester.view.reset);
  await tester.pumpWidget(UncontrolledProviderScope(container: c, child: MaterialApp(home: Scaffold(body: home))));
  await tester.pumpAndSettle();
  return c;
}

void main() {
  test('the starter list holds no one\'s household schedule', () {
    for (final c in starterChores) {
      for (final w in _owner) {
        expect(c.name.toLowerCase(), isNot(contains(w.toLowerCase())), reason: c.name);
      }
    }
    expect(starterChores, isNotEmpty);
  });

  testWidgets('an empty Today offers creating your own item first, and generic examples second', (tester) async {
    final c = await pump(tester, const TodayScreen());
    expect(find.text('Nothing here yet'), findsOneWidget);
    expect(find.text('Create your first item'), findsOneWidget);
    expect(find.text('Add a few examples'), findsOneWidget);
    expect(find.text('Add starter chores'), findsNothing);
    final text = tester.widgetList<Text>(find.byType(Text)).map((t) => t.data ?? '').join(' ');
    for (final w in _owner) {
      expect(text, isNot(contains(w)));
    }
    // Nothing was added behind the user's back.
    expect(await c.read(dbProvider).select(c.read(dbProvider).items).get(), isEmpty);

    await tester.tap(find.text('Create your first item'));
    await tester.pumpAndSettle();
    expect(find.text('Save'), findsOneWidget, reason: 'the item editor opened');
  });

  testWidgets('the examples are generic and only added when asked for', (tester) async {
    final c = await pump(tester, const TodayScreen());
    await tester.tap(find.text('Add a few examples'));
    await tester.pumpAndSettle();
    await tester.pump(const Duration(seconds: 3)); // saving schedules a sync; let that timer fire
    final rows = await c.read(dbProvider).select(c.read(dbProvider).items).get();
    expect(rows, hasLength(starterChores.length));
    for (final r in rows) {
      for (final w in _owner) {
        expect(r.name.toLowerCase(), isNot(contains(w.toLowerCase())));
      }
    }
  });

  testWidgets('the pairing screen no longer tells everyone to use the owner\'s server', (tester) async {
    await pump(tester, const PairScreen());
    await tester.tap(find.text('Pair with a code'));
    await tester.pumpAndSettle();
    expect(find.textContaining('Hestia'), findsNothing);
    expect(find.textContaining('docker exec'), findsNothing);
    expect(find.textContaining('Add another device'), findsOneWidget);
  });
}
