import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/database.dart';
import '../rules.dart';
import '../state.dart';
import '../theme.dart';
import 'widgets.dart';

class TodosScreen extends ConsumerStatefulWidget {
  const TodosScreen({super.key});
  @override
  ConsumerState<TodosScreen> createState() => _TodosScreenState();
}

class _TodosScreenState extends ConsumerState<TodosScreen> {
  final _text = TextEditingController();
  DateTime? _due;

  @override
  void dispose() {
    _text.dispose();
    super.dispose();
  }

  Future<void> _add() async {
    final name = _text.text.trim();
    if (name.isEmpty) return;
    await ref
        .read(controllerProvider)
        .saveItem(
          kind: 'todo',
          name: name,
          weekdays: 0,
          creating: true,
          dueDate: _due == null ? null : fmtDay(_due!),
        );
    setState(() {
      _text.clear();
      _due = null;
    });
  }

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final items = (ref.watch(itemsProvider).value ?? const <ItemRow>[])
        .where((i) => i.kind == 'todo' && i.active)
        .toList();
    final comps =
        ref.watch(completionsProvider).value ?? const <CompletionRow>[];
    final doneIds = {for (final c in comps) c.itemId};
    final today = ref.watch(todayProvider);
    final ctl = ref.read(controllerProvider);

    final open = items.where((i) => !doneIds.contains(i.id)).toList()
      ..sort((a, b) => (a.dueDate ?? '9999').compareTo(b.dueDate ?? '9999'));
    final finished = items.where((i) => doneIds.contains(i.id)).toList();

    Widget row(ItemRow t) {
      final done = doneIds.contains(t.id);
      final due = t.dueDate == null ? null : parseDay(t.dueDate!);
      final soon = due != null && !done && due.difference(today).inDays <= 2;
      return Padding(
        padding: const EdgeInsets.only(bottom: 8),
        child: Dismissible(
          key: ValueKey(t.id),
          direction: DismissDirection.endToStart,
          background: Container(
            alignment: Alignment.centerRight,
            padding: const EdgeInsets.only(right: 20),
            decoration: BoxDecoration(
              color: p.pink.withValues(alpha: .25),
              borderRadius: BorderRadius.circular(8),
            ),
            child: Icon(Icons.delete_outline, color: p.pink),
          ),
          onDismissed: (_) => ctl.deleteItem(t.id),
          child: TickRow(
            title: t.name,
            done: done,
            // To-dos are done once; the completion date is the day they were ticked.
            onTap: (pos) => ctl.toggle(t, today, at: pos),
            trailing: due == null
                ? null
                : Container(
                    padding: const EdgeInsets.symmetric(
                      horizontal: 8,
                      vertical: 2,
                    ),
                    decoration: BoxDecoration(
                      color: soon ? p.accent.withValues(alpha: .22) : p.raised,
                      borderRadius: BorderRadius.circular(999),
                    ),
                    child: Text(
                      '${dayShort[weekdayIndex(due)]} ${due.day} ${monthShort[due.month - 1]}',
                      style: TextStyle(
                        fontSize: 12,
                        color: soon ? p.accent : p.dim,
                      ),
                    ),
                  ),
          ),
        ),
      );
    }

    return SafeArea(
      bottom: false,
      child: ListView(
        padding: const EdgeInsets.fromLTRB(16, 20, 16, 24),
        children: [
          const Text(
            'To-dos',
            style: TextStyle(fontSize: 22, fontWeight: FontWeight.w700),
          ),
          const SizedBox(height: 12),
          Row(
            children: [
              Expanded(
                child: TextField(
                  controller: _text,
                  textInputAction: TextInputAction.done,
                  onSubmitted: (_) => _add(),
                  decoration: const InputDecoration(hintText: 'Add a task'),
                ),
              ),
              const SizedBox(width: 8),
              IconButton.filledTonal(
                tooltip: 'Due date',
                onPressed: () async {
                  final d = await showDatePicker(
                    context: context,
                    initialDate: _due ?? DateTime.now(),
                    firstDate: DateTime.now().subtract(const Duration(days: 1)),
                    lastDate: DateTime.now().add(const Duration(days: 730)),
                  );
                  if (d != null) {
                    setState(() => _due = DateTime.utc(d.year, d.month, d.day));
                  }
                },
                icon: Icon(Icons.event, color: _due == null ? p.dim : p.accent),
              ),
              const SizedBox(width: 4),
              FilledButton(onPressed: _add, child: const Text('Add')),
            ],
          ),
          if (_due != null)
            Padding(
              padding: const EdgeInsets.only(top: 8),
              child: Text(
                'Due ${dayShort[weekdayIndex(_due!)]} ${_due!.day} ${monthShort[_due!.month - 1]}',
                style: TextStyle(color: p.accent, fontSize: 12),
              ),
            ),
          SectionHeader('Open', trailing: '${open.length}'),
          if (open.isEmpty)
            Text(
              'Nothing open. Add something above.',
              style: TextStyle(color: p.dim),
            ),
          for (final t in open) row(t),
          if (finished.isNotEmpty)
            SectionHeader('Done', trailing: '${finished.length}'),
          for (final t in finished) row(t),
        ],
      ),
    );
  }
}
