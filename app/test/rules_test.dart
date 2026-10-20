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
}
