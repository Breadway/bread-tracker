import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/database.dart';
import '../fx/sfx.dart';
import '../rules.dart';
import '../state.dart';
import '../theme.dart';
import 'widgets.dart';

class TodayScreen extends ConsumerWidget {
  const TodayScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final p = palOf(context);
    final items = ref.watch(itemsProvider).value ?? const <ItemRow>[];
    final comps =
        ref.watch(completionsProvider).value ?? const <CompletionRow>[];
    final today = ref.watch(todayProvider);
    final selected = ref.watch(selectedDayProvider);
    final stats = ref.watch(statsProvider);
    final ctl = ref.read(controllerProvider);
    final sfx = ref.watch(sfxProvider);

    final sel = fmtDay(selected);
    final doneIds = {
      for (final c in comps.where((c) => c.date == sel)) c.itemId,
    };
    bool sched(ItemRow i) =>
        i.active && (i.weekdays & (1 << weekdayIndex(selected))) != 0;
    final chores = items.where((i) => i.kind == 'chore' && sched(i)).toList()
      ..sort(
        (a, b) => (a.remindTime ?? '99:99').compareTo(b.remindTime ?? '99:99'),
      );
    final habits = items.where((i) => i.kind == 'habit' && sched(i)).toList();

    final total = chores.length + habits.length;
    final done = [
      ...chores,
      ...habits,
    ].where((i) => doneIds.contains(i.id)).length;

    final monday = today.subtract(Duration(days: weekdayIndex(today)));
    final week = [for (var i = 0; i < 7; i++) monday.add(Duration(days: i))];

    String dayFullFor(DateTime d) => dayFull[weekdayIndex(d)];

    return SafeArea(
      bottom: false,
      child: ListView(
        padding: const EdgeInsets.fromLTRB(16, 20, 16, 24),
        children: [
          Row(
            children: [
              ProgressRing(done: done, total: total),
              const SizedBox(width: 16),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(
                      dayFullFor(selected),
                      style: const TextStyle(
                        fontSize: 22,
                        fontWeight: FontWeight.w700,
                      ),
                    ),
                    const SizedBox(height: 4),
                    Text(
                      '${selected.day} ${monthShort[selected.month - 1]}${selected == today ? ' · today' : ''} · $done of $total done',
                      style: TextStyle(color: p.dim, fontSize: 12),
                    ),
                  ],
                ),
              ),
            ],
          ),
          const SizedBox(height: 12),
          XpBar(
            level: stats.level,
            rank: stats.rank,
            into: stats.intoLevel,
            per: xpPerLevel,
            trailing: _SoundButton(sfx: sfx),
          ),
          const SizedBox(height: 12),
          Row(
            children: [
              for (final d in week)
                Expanded(
                  child: Padding(
                    padding: const EdgeInsets.symmetric(horizontal: 2),
                    child: _DayChip(
                      day: d,
                      selected: d == selected,
                      allDone: _allDone(items, comps, d),
                      onTap: () =>
                          ref.read(selectedDayProvider.notifier).select(d),
                    ),
                  ),
                ),
            ],
          ),
          if (items.isEmpty) ...[
            const SizedBox(height: 32),
            Panel(
              padding: const EdgeInsets.all(20),
              child: Column(
                children: [
                  Text(
                    'Nothing here yet',
                    style: TextStyle(
                      color: p.fg,
                      fontWeight: FontWeight.w700,
                      fontSize: 16,
                    ),
                  ),
                  const SizedBox(height: 8),
                  Text(
                    'Add your weekly chores in one tap, or create your own chores and habits in Settings.',
                    textAlign: TextAlign.center,
                    style: TextStyle(color: p.dim),
                  ),
                  const SizedBox(height: 16),
                  FilledButton(
                    onPressed: () async {
                      final n = await ctl.addStarterChores();
                      if (context.mounted) {
                        ScaffoldMessenger.of(context).showSnackBar(
                          SnackBar(content: Text('Added $n chores')),
                        );
                      }
                    },
                    child: const Text('Add starter chores'),
                  ),
                ],
              ),
            ),
          ] else ...[
            SectionHeader(
              'Chores',
              trailing:
                  '${chores.where((c) => doneIds.contains(c.id)).length} of ${chores.length}',
            ),
            if (chores.isEmpty)
              Text(
                'No chores on ${dayFullFor(selected)}.',
                style: TextStyle(color: p.dim),
              ),
            for (final c in chores)
              Padding(
                padding: const EdgeInsets.only(bottom: 8),
                child: TickRow(
                  title: c.name,
                  done: doneIds.contains(c.id),
                  onTap: (pos) => ctl.toggle(c, selected, at: pos),
                  trailing: c.remindTime == null
                      ? null
                      : Text(
                          fmtTime(c.remindTime!),
                          style: TextStyle(color: p.dim, fontSize: 12),
                        ),
                ),
              ),
            SectionHeader(
              'Habits',
              trailing:
                  '${habits.where((h) => doneIds.contains(h.id)).length} of ${habits.length}',
            ),
            if (habits.isEmpty)
              Text(
                'No habits yet. Add one in Settings, then Manage items.',
                style: TextStyle(color: p.dim),
              ),
            for (final h in habits)
              Padding(
                padding: const EdgeInsets.only(bottom: 8),
                child: _HabitCard(
                  habit: h,
                  done: doneIds.contains(h.id),
                  streak: stats.streaks[h.id] ?? 0,
                  weekDone: [
                    for (final d in week)
                      comps.any((c) => c.itemId == h.id && c.date == fmtDay(d)),
                  ],
                  selectedIndex: weekdayIndex(selected),
                  onTap: (pos) => ctl.toggle(h, selected, at: pos),
                ),
              ),
          ],
        ],
      ),
    );
  }

  bool _allDone(List<ItemRow> items, List<CompletionRow> comps, DateTime d) {
    final ds = fmtDay(d);
    final scheduled = items.where(
      (i) =>
          i.active &&
          (i.kind == 'chore' || i.kind == 'habit') &&
          (i.weekdays & (1 << weekdayIndex(d))) != 0,
    );
    if (scheduled.isEmpty) return false;
    final done = {for (final c in comps.where((c) => c.date == ds)) c.itemId};
    return scheduled.every((i) => done.contains(i.id));
  }
}

class _SoundButton extends StatefulWidget {
  const _SoundButton({required this.sfx});
  final Sfx sfx;
  @override
  State<_SoundButton> createState() => _SoundButtonState();
}

class _SoundButtonState extends State<_SoundButton> {
  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final on = widget.sfx.sound;
    return Semantics(
      button: true,
      label: 'Sound effects',
      toggled: on,
      child: InkResponse(
        onTap: () => setState(() => widget.sfx.sound = !on),
        radius: 22,
        child: Container(
          width: 32,
          height: 32,
          decoration: BoxDecoration(color: p.raised, shape: BoxShape.circle),
          child: Icon(
            on ? Icons.volume_up_rounded : Icons.volume_off_rounded,
            size: 16,
            color: on ? p.accent : p.dim,
          ),
        ),
      ),
    );
  }
}

class _DayChip extends StatelessWidget {
  const _DayChip({
    required this.day,
    required this.selected,
    required this.allDone,
    required this.onTap,
  });
  final DateTime day;
  final bool selected, allDone;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final fg = selected ? p.onAccent : p.fg;
    return Semantics(
      button: true,
      selected: selected,
      label: '${dayFull[weekdayIndex(day)]} ${day.day}',
      child: GestureDetector(
        onTap: onTap,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 180),
          padding: const EdgeInsets.symmetric(vertical: 8),
          decoration: BoxDecoration(
            color: selected ? p.accent : p.surface,
            border: Border.all(color: selected ? p.accent : p.line),
            borderRadius: BorderRadius.circular(8),
          ),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                dayShort[weekdayIndex(day)],
                style: TextStyle(
                  fontSize: 12,
                  color: selected ? p.onAccent.withValues(alpha: .8) : p.dim,
                ),
              ),
              const SizedBox(height: 4),
              Text(
                '${day.day}',
                style: TextStyle(
                  fontSize: 14,
                  fontWeight: FontWeight.w700,
                  color: fg,
                ),
              ),
              const SizedBox(height: 4),
              Container(
                width: allDone ? 8 : 6,
                height: allDone ? 8 : 6,
                decoration: BoxDecoration(
                  shape: BoxShape.circle,
                  color: allDone ? p.good : p.line,
                  boxShadow: allDone
                      ? [
                          BoxShadow(
                            color: p.good.withValues(alpha: .8),
                            blurRadius: 8,
                          ),
                        ]
                      : null,
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _HabitCard extends StatelessWidget {
  const _HabitCard({
    required this.habit,
    required this.done,
    required this.streak,
    required this.weekDone,
    required this.selectedIndex,
    required this.onTap,
  });
  final ItemRow habit;
  final bool done;
  final int streak, selectedIndex;
  final List<bool> weekDone;
  final void Function(Offset) onTap;

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final hot = streak >= 3;
    return Panel(
      highlight: hot,
      child: Column(
        children: [
          TickRow(
            bare: true,
            title: habit.name,
            done: done,
            onTap: onTap,
            trailing: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                if (streak >= 2) _Flame(hot: hot),
                if (streak >= 2) const SizedBox(width: 4),
                Text(
                  streak == 0
                      ? 'no streak'
                      : '$streak ${streak == 1 ? 'day' : 'days'}',
                  style: TextStyle(
                    color: hot ? p.gold : p.accent,
                    fontSize: 12,
                    fontWeight: hot ? FontWeight.w700 : FontWeight.w400,
                  ),
                ),
              ],
            ),
          ),
          const SizedBox(height: 8),
          Row(
            children: [
              for (var i = 0; i < 7; i++)
                Expanded(
                  child: Padding(
                    padding: const EdgeInsets.symmetric(horizontal: 2),
                    child: Container(
                      height: 6,
                      decoration: BoxDecoration(
                        borderRadius: BorderRadius.circular(3),
                        gradient: weekDone[i]
                            ? LinearGradient(colors: [p.accent, p.gold])
                            : null,
                        color: weekDone[i] ? null : p.raised,
                        border: i == selectedIndex
                            ? Border.all(color: p.dim, width: 1)
                            : null,
                      ),
                    ),
                  ),
                ),
            ],
          ),
        ],
      ),
    );
  }
}

class _Flame extends StatefulWidget {
  const _Flame({required this.hot});
  final bool hot;
  @override
  State<_Flame> createState() => _FlameState();
}

class _FlameState extends State<_Flame> with SingleTickerProviderStateMixin {
  late final AnimationController _c = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 1100),
  );

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (widget.hot && !MediaQuery.disableAnimationsOf(context)) {
      if (!_c.isAnimating) _c.repeat(reverse: true);
    } else {
      _c.stop();
    }
  }

  @override
  void dispose() {
    _c.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return AnimatedBuilder(
      animation: _c,
      builder: (_, _) => Transform.rotate(
        angle: -.09 * _c.value,
        alignment: Alignment.bottomCenter,
        child: Transform.scale(
          scale: 1 + .18 * _c.value,
          alignment: Alignment.bottomCenter,
          child: Icon(
            Icons.local_fire_department_rounded,
            size: 16,
            color: widget.hot ? p.gold : p.accent,
          ),
        ),
      ),
    );
  }
}
