/// XP, level, perfect-day and streak rules. Mirrors server/src/xp.rs; both run the
/// shared vectors in spec/xp_vectors.json so they cannot drift. See DESIGN.md.
library;

const perfectDayBonus = 50;
const xpPerLevel = 120;
const ranks = ['Flour', 'Dough', 'Proofing', 'Rising', 'Baked', 'Golden Crust', 'Sourdough Master'];

String rankFor(int level) => ranks[(level - 1).clamp(0, ranks.length - 1)];

class RItem {
  const RItem({
    required this.id,
    required this.kind,
    required this.weekdays,
    required this.points,
    required this.active,
    this.deleted = false,
  });
  final String id, kind;
  final int weekdays, points;
  final bool active, deleted;
}

class RCompletion {
  const RCompletion(this.itemId, this.date, {this.deleted = false});
  final String itemId, date;
  final bool deleted;
}

class Stats {
  const Stats({
    required this.xp,
    required this.level,
    required this.rank,
    required this.intoLevel,
    required this.perfectDays,
    required this.streaks,
  });
  final int xp, level, intoLevel;
  final String rank;
  final List<String> perfectDays;
  final Map<String, int> streaks;
}

DateTime parseDay(String s) {
  final p = s.split('-');
  return DateTime.utc(int.parse(p[0]), int.parse(p[1]), int.parse(p[2]));
}

String fmtDay(DateTime d) =>
    '${d.year.toString().padLeft(4, '0')}-${d.month.toString().padLeft(2, '0')}-${d.day.toString().padLeft(2, '0')}';

/// Monday = 0 .. Sunday = 6.
int weekdayIndex(DateTime d) => d.weekday - 1;

bool scheduledOn(RItem i, DateTime date) =>
    i.active && (i.kind == 'chore' || i.kind == 'habit') && (i.weekdays & (1 << weekdayIndex(date))) != 0;

Stats computeStats(List<RItem> items, List<RCompletion> completions, DateTime today) {
  final live = {for (final i in items.where((i) => !i.deleted)) i.id: i};
  final done = <String, Set<String>>{};
  var xp = 0;
  for (final c in completions.where((c) => !c.deleted)) {
    final item = live[c.itemId];
    if (item == null) continue;
    if (done.putIfAbsent(item.id, () => {}).add(c.date)) xp += item.points;
  }

  final dates = <String>{for (final s in done.values) ...s};
  final perfect = <String>[];
  for (final ds in dates.toList()..sort()) {
    final date = parseDay(ds);
    final scheduled = live.values.where((i) => scheduledOn(i, date)).toList();
    if (scheduled.isNotEmpty && scheduled.every((i) => done[i.id]?.contains(ds) ?? false)) {
      xp += perfectDayBonus;
      perfect.add(ds);
    }
  }

  final streaks = <String, int>{};
  final todayStr = fmtDay(today);
  for (final item in live.values.where((i) => i.kind == 'habit')) {
    final set = done[item.id];
    var day = (set?.contains(todayStr) ?? false) ? today : today.subtract(const Duration(days: 1));
    var n = 0;
    while (set?.contains(fmtDay(day)) ?? false) {
      n++;
      day = day.subtract(const Duration(days: 1));
    }
    streaks[item.id] = n;
  }

  final level = xp ~/ xpPerLevel + 1;
  return Stats(
    xp: xp,
    level: level,
    rank: rankFor(level),
    intoLevel: xp % xpPerLevel,
    perfectDays: perfect,
    streaks: streaks,
  );
}
