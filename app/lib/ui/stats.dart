import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/database.dart';
import '../rules.dart';
import '../state.dart';
import '../theme.dart';
import 'widgets.dart';

class StatsScreen extends ConsumerWidget {
  const StatsScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final p = palOf(context);
    final items = ref.watch(itemsProvider).value ?? const <ItemRow>[];
    final comps =
        ref.watch(completionsProvider).value ?? const <CompletionRow>[];
    final today = ref.watch(todayProvider);
    final stats = ref.watch(statsProvider);

    final doneByDate = <String, Set<String>>{};
    for (final c in comps) {
      doneByDate.putIfAbsent(c.date, () => {}).add(c.itemId);
    }

    double frac(DateTime d) {
      final scheduled = items
          .where(
            (i) =>
                i.active &&
                (i.kind == 'chore' || i.kind == 'habit') &&
                (i.weekdays & (1 << weekdayIndex(d))) != 0,
          )
          .toList();
      if (scheduled.isEmpty) return 0;
      final done = doneByDate[fmtDay(d)] ?? const <String>{};
      return scheduled.where((i) => done.contains(i.id)).length /
          scheduled.length;
    }

    final monday = today.subtract(Duration(days: weekdayIndex(today)));
    final week = [for (var i = 0; i < 7; i++) monday.add(Duration(days: i))];
    final elapsed = week.where((d) => !d.isAfter(today)).toList();
    final weekPct = elapsed.isEmpty
        ? 0
        : (elapsed.map(frac).reduce((a, b) => a + b) / elapsed.length * 100)
              .round();
    final best = stats.streaks.values.fold<int>(0, (a, b) => a > b ? a : b);
    final doneToday = (doneByDate[fmtDay(today)] ?? const <String>{}).length;

    return SafeArea(
      bottom: false,
      child: ListView(
        padding: const EdgeInsets.fromLTRB(16, 20, 16, 24),
        children: [
          const Text(
            'Stats',
            style: TextStyle(fontSize: 22, fontWeight: FontWeight.w700),
          ),
          const SizedBox(height: 12),
          Row(
            children: [
              Expanded(child: _Kpi('$weekPct%', 'this week')),
              const SizedBox(width: 8),
              Expanded(child: _Kpi('$best', 'best streak')),
              const SizedBox(width: 8),
              Expanded(child: _Kpi('$doneToday', 'done today')),
            ],
          ),
          const SizedBox(height: 8),
          Panel(
            padding: const EdgeInsets.all(16),
            child: Row(
              children: [
                Icon(Icons.workspace_premium_rounded, color: p.gold),
                const SizedBox(width: 12),
                Expanded(
                  child: Text(
                    '${stats.xp} XP · level ${stats.level} · ${stats.rank}\n${stats.perfectDays.length} perfect ${stats.perfectDays.length == 1 ? 'day' : 'days'}',
                    style: const TextStyle(height: 1.4),
                  ),
                ),
              ],
            ),
          ),
          const SectionHeader('Completion by day'),
          Panel(
            padding: const EdgeInsets.all(16),
            child: SizedBox(
              height: 120,
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.end,
                children: [
                  for (final d in week)
                    Expanded(
                      child: _Bar(
                        label: dayShort[weekdayIndex(d)],
                        value: d.isAfter(today) ? null : frac(d),
                        today: d == today,
                      ),
                    ),
                ],
              ),
            ),
          ),
          const SectionHeader('Last 5 weeks'),
          Panel(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                for (var w = 4; w >= 0; w--)
                  Padding(
                    padding: const EdgeInsets.only(bottom: 4),
                    child: Row(
                      children: [
                        for (var i = 0; i < 7; i++)
                          Expanded(
                            child: Padding(
                              padding: const EdgeInsets.symmetric(
                                horizontal: 2,
                              ),
                              child: _Cell(() {
                                final d = monday
                                    .subtract(Duration(days: 7 * w))
                                    .add(Duration(days: i));
                                return d.isAfter(today) ? null : frac(d);
                              }()),
                            ),
                          ),
                      ],
                    ),
                  ),
                const SizedBox(height: 4),
                Text(
                  'Mon to Sun, oldest week at the top. Brighter means more done.',
                  style: TextStyle(color: p.dim, fontSize: 12),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class _Kpi extends StatelessWidget {
  const _Kpi(this.value, this.label);
  final String value, label;
  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return Panel(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            value,
            style: const TextStyle(
              fontSize: 22,
              fontWeight: FontWeight.w700,
              fontFeatures: [FontFeature.tabularFigures()],
            ),
          ),
          const SizedBox(height: 4),
          Text(label, style: TextStyle(color: p.dim, fontSize: 12)),
        ],
      ),
    );
  }
}

class _Bar extends StatelessWidget {
  const _Bar({required this.label, required this.value, required this.today});
  final String label;
  final double? value;
  final bool today;
  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final v = value ?? 0;
    return Column(
      mainAxisAlignment: MainAxisAlignment.end,
      children: [
        Text(
          value == null ? '' : '${(v * 100).round()}',
          style: TextStyle(color: p.dim, fontSize: 12),
        ),
        const SizedBox(height: 4),
        TweenAnimationBuilder<double>(
          tween: Tween(begin: 0, end: v),
          duration: const Duration(milliseconds: 500),
          curve: Curves.easeOut,
          builder: (_, t, _) => Container(
            width: 24,
            height: (t * 70).clamp(3, 70),
            decoration: BoxDecoration(
              color: today ? p.good : p.accent,
              borderRadius: const BorderRadius.vertical(
                top: Radius.circular(4),
                bottom: Radius.circular(2),
              ),
            ),
          ),
        ),
        const SizedBox(height: 4),
        Text(label, style: TextStyle(color: p.dim, fontSize: 12)),
      ],
    );
  }
}

class _Cell extends StatelessWidget {
  const _Cell(this.v);
  final double? v;
  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return AspectRatio(
      aspectRatio: 1,
      child: Container(
        decoration: BoxDecoration(
          borderRadius: BorderRadius.circular(4),
          color: v == null
              ? p.raised.withValues(alpha: .4)
              : Color.lerp(p.raised, p.accent, .12 + v! * .88),
        ),
      ),
    );
  }
}
