import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:tracker/rules.dart';

void main() {
  test('shared xp vectors match the server rules', () {
    final cases = jsonDecode(File('../spec/xp_vectors.json').readAsStringSync()) as List;
    expect(cases, isNotEmpty);
    for (final c in cases) {
      final items = [
        for (final i in c['items'] as List)
          RItem(
            id: i['id'],
            kind: i['kind'],
            weekdays: i['weekdays'],
            points: i['points'],
            active: i['active'],
            deleted: i['deleted_at'] != null,
            createdOn: i['created_on'],
          ),
      ];
      final comps = [
        for (final k in c['completions'] as List) RCompletion(k['item_id'], k['date'], deleted: k['deleted_at'] != null),
      ];
      final s = computeStats(items, comps, parseDay(c['today']));
      final e = c['expect'];
      expect(s.xp, e['xp'], reason: '${c['name']}: xp');
      expect(s.level, e['level'], reason: '${c['name']}: level');
      expect(s.perfectDays, List<String>.from(e['perfect_days']), reason: '${c['name']}: perfect days');
      expect(s.streaks, Map<String, int>.from(e['streaks']), reason: '${c['name']}: streaks');
    }
  });

  test('ranks clamp', () {
    expect(rankFor(1), 'Flour');
    expect(rankFor(7), 'Sourdough Master');
    expect(rankFor(99), 'Sourdough Master');
  });

  test('adding an item created today never reduces xp or perfect days', () {
    final today = parseDay('2026-10-20');
    final items = [
      const RItem(
        id: 'a',
        kind: 'chore',
        weekdays: 127,
        points: 10,
        active: true,
        createdOn: '2026-10-01',
      ),
    ];
    final comps = [
      for (final d in ['2026-10-18', '2026-10-19', '2026-10-20'])
        RCompletion('a', d),
    ];
    final before = computeStats(items, comps, today);
    expect(before.perfectDays, hasLength(3));

    for (final created in ['2026-10-20', '2026-10-27']) {
      final after = computeStats([
        ...items,
        RItem(
          id: 'b',
          kind: 'chore',
          weekdays: 127,
          points: 10,
          active: true,
          createdOn: created,
        ),
      ], comps, today);
      expect(
        after.xp,
        greaterThanOrEqualTo(before.xp),
        reason: 'adding an item created $created reduced xp',
      );
      expect(
        after.perfectDays.length,
        greaterThanOrEqualTo(before.perfectDays.length),
        reason: 'adding an item created $created removed a perfect day',
      );
    }
  });

  test('an item counts from the day after creation', () {
    final items = [
      const RItem(
        id: 'a',
        kind: 'chore',
        weekdays: 127,
        points: 10,
        active: true,
        createdOn: '2026-10-20',
      ),
    ];
    final sameDay = computeStats(items, [
      const RCompletion('a', '2026-10-20'),
    ], parseDay('2026-10-20'));
    expect(sameDay.xp, 10, reason: 'the completion still earns its item XP');
    expect(sameDay.perfectDays, isEmpty);

    final nextDay = computeStats(items, [
      const RCompletion('a', '2026-10-20'),
      const RCompletion('a', '2026-10-21'),
    ], parseDay('2026-10-21'));
    expect(nextDay.perfectDays, ['2026-10-21']);
  });
}
