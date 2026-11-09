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
            createdBy: i['created_by'] as int?,
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

  test('parent-assigned items score exactly like owned items', () {
    final today = parseDay('2026-10-19');
    final variants = [
      for (final createdBy in [null, 1, 2])
        [
          RItem(
            id: 'chore',
            kind: 'chore',
            weekdays: 127,
            points: 10,
            active: true,
            createdOn: '2026-10-01',
            createdBy: createdBy,
          ),
          RItem(
            id: 'habit',
            kind: 'habit',
            weekdays: 127,
            points: 15,
            active: true,
            createdOn: '2026-10-01',
            createdBy: createdBy,
          ),
        ],
    ];
    final comps = [
      const RCompletion('chore', '2026-10-19'),
      for (final d in ['2026-10-17', '2026-10-18', '2026-10-19'])
        RCompletion('habit', d),
    ];

    final scores = [
      for (final set in variants) computeStats(set, comps, today),
    ];
    for (final s in scores) {
      expect(s.xp, 105, reason: 'chore points, habit points and perfect-day bonus');
      expect(s.perfectDays, ['2026-10-19']);
      expect(s.streaks, {'habit': 3});
    }
    expect(scores[1].xp, scores[0].xp);
    expect(scores[2].xp, scores[0].xp);
    expect(scores[1].perfectDays, scores[0].perfectDays);
    expect(scores[2].perfectDays, scores[0].perfectDays);
    expect(scores[1].streaks, scores[0].streaks);
    expect(scores[2].streaks, scores[0].streaks);
  });

  test('a parent-assigned item follows the creation-date bound', () {
    final parentChore = RItem(
      id: 'parent',
      kind: 'chore',
      weekdays: 127,
      points: 10,
      active: true,
      createdOn: '2026-10-19',
      createdBy: 2,
    );
    final sameDay = computeStats([parentChore], [
      const RCompletion('parent', '2026-10-19'),
    ], parseDay('2026-10-19'));
    expect(sameDay.xp, 10, reason: 'the completion still earns item XP');
    expect(sameDay.perfectDays, isEmpty, reason: 'the creation day is not scheduled');

    final nextDay = computeStats([parentChore], [
      const RCompletion('parent', '2026-10-20'),
    ], parseDay('2026-10-20'));
    expect(nextDay.xp, 60, reason: '10 points plus the perfect-day bonus');
    expect(nextDay.perfectDays, ['2026-10-20']);
  });
}
