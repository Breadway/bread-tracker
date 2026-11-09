import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/database.dart';
import '../rules.dart';
import '../state.dart';
import '../theme.dart';
import 'widgets.dart';

String weekdaysLabel(int mask) {
  if (mask == 127) return 'Every day';
  if (mask == 0) return 'No days';
  return [
    for (var i = 0; i < 7; i++)
      if (mask & (1 << i) != 0) dayShort[i],
  ].join(' ');
}

class ItemsScreen extends ConsumerWidget {
  const ItemsScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final p = palOf(context);
    final items = ref.watch(itemsProvider).value ?? const <ItemRow>[];
    Widget group(String title, String kind) {
      final list = items.where((i) => i.kind == kind).toList();
      return Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SectionHeader(title, trailing: '${list.length}'),
          if (list.isEmpty) Text('None yet.', style: TextStyle(color: p.dim)),
          for (final i in list)
            Padding(
              padding: const EdgeInsets.only(bottom: 8),
              child: GestureDetector(
                onTap: () => showItemEditor(context, ref, item: i),
                child: Panel(
                  child: Row(
                    children: [
                      Expanded(
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          children: [
                            Text(
                              i.name,
                              style: TextStyle(color: i.active ? p.fg : p.dim),
                            ),
                            Text(
                              [
                                if (kind != 'todo') weekdaysLabel(i.weekdays),
                                if (i.remindTime != null)
                                  'reminder ${fmtTime(i.remindTime!)}',
                                if (i.dueDate != null) 'due ${i.dueDate}',
                                '${i.points} XP',
                                if (!i.active) 'paused',
                              ].join(' · '),
                              style: TextStyle(color: p.dim, fontSize: 12),
                            ),
                          ],
                        ),
                      ),
                      Icon(Icons.chevron_right, color: p.dim),
                    ],
                  ),
                ),
              ),
            ),
        ],
      );
    }

    return Scaffold(
      appBar: AppBar(title: const Text('Manage items'), backgroundColor: p.bg),
      floatingActionButton: FloatingActionButton.extended(
        onPressed: () => showItemEditor(context, ref),
        backgroundColor: p.accent,
        foregroundColor: p.onAccent,
        icon: const Icon(Icons.add),
        label: const Text('New item'),
      ),
      body: ListView(
        padding: const EdgeInsets.fromLTRB(16, 0, 16, 96),
        children: [
          group('Chores', 'chore'),
          group('Habits', 'habit'),
          group('To-dos', 'todo'),
        ],
      ),
    );
  }
}

Future<void> showItemEditor(
  BuildContext context,
  WidgetRef ref, {
  ItemRow? item,
}) {
  return showModalBottomSheet<void>(
    context: context,
    isScrollControlled: true,
    backgroundColor: palOf(context).surface,
    shape: const RoundedRectangleBorder(
      borderRadius: BorderRadius.vertical(top: Radius.circular(16)),
    ),
    builder: (_) => Padding(
      padding: EdgeInsets.only(bottom: MediaQuery.viewInsetsOf(context).bottom),
      child: _ItemEditor(item: item),
    ),
  );
}

class _ItemEditor extends ConsumerStatefulWidget {
  const _ItemEditor({this.item});
  final ItemRow? item;
  @override
  ConsumerState<_ItemEditor> createState() => _ItemEditorState();
}

class _ItemEditorState extends ConsumerState<_ItemEditor> {
  late final TextEditingController _name = TextEditingController(
    text: widget.item?.name ?? '',
  );
  late final TextEditingController _points = TextEditingController(
    text: '${widget.item?.points ?? 10}',
  );
  late String _kind = widget.item?.kind ?? 'chore';
  late int _mask = widget.item?.weekdays ?? 127;
  late String? _time = widget.item?.remindTime;
  late String? _due = widget.item?.dueDate;
  late bool _active = widget.item?.active ?? true;
  String? _error;

  static const _defaults = {'chore': 10, 'habit': 15, 'todo': 20};

  @override
  void dispose() {
    _name.dispose();
    _points.dispose();
    super.dispose();
  }

  Future<void> _save() async {
    final name = _name.text.trim();
    final points = int.tryParse(_points.text.trim());
    if (name.isEmpty || name.length > 200) {
      return setState(() => _error = 'Give it a name (up to 200 characters).');
    }
    if (points == null || points < 0 || points > 1000) {
      return setState(() => _error = 'XP must be a number from 0 to 1000.');
    }
    await ref
        .read(controllerProvider)
        .saveItem(
          id: widget.item?.id,
          creating: widget.item == null,
          kind: _kind,
          name: name,
          weekdays: _kind == 'todo' ? 0 : _mask,
          remindTime: _time,
          points: points,
          dueDate: _kind == 'todo' ? _due : null,
          active: _active,
        );
    if (mounted) Navigator.pop(context);
  }

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final editing = widget.item != null;
    return SafeArea(
      child: SingleChildScrollView(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(
              editing ? 'Edit item' : 'New item',
              style: const TextStyle(fontSize: 18, fontWeight: FontWeight.w700),
            ),
            const SizedBox(height: 16),
            TextField(
              controller: _name,
              decoration: const InputDecoration(labelText: 'Name'),
              textCapitalization: TextCapitalization.sentences,
            ),
            const SizedBox(height: 12),
            SegmentedButton<String>(
              segments: const [
                ButtonSegment(value: 'chore', label: Text('Chore')),
                ButtonSegment(value: 'habit', label: Text('Habit')),
                ButtonSegment(value: 'todo', label: Text('To-do')),
              ],
              selected: {_kind},
              onSelectionChanged: editing
                  ? null
                  : (s) => setState(() {
                      _kind = s.first;
                      _points.text = '${_defaults[_kind]}';
                    }),
            ),
            if (_kind != 'todo') ...[
              const SizedBox(height: 16),
              Text('Days', style: TextStyle(color: p.dim, fontSize: 12)),
              const SizedBox(height: 8),
              Wrap(
                spacing: 8,
                runSpacing: 8,
                children: [
                  for (var i = 0; i < 7; i++)
                    FilterChip(
                      label: Text(dayShort[i]),
                      selected: _mask & (1 << i) != 0,
                      onSelected: (on) => setState(
                        () => _mask = on ? _mask | (1 << i) : _mask & ~(1 << i),
                      ),
                    ),
                ],
              ),
            ] else ...[
              const SizedBox(height: 16),
              OutlinedButton.icon(
                onPressed: () async {
                  final d = await showDatePicker(
                    context: context,
                    initialDate: _due == null
                        ? DateTime.now()
                        : parseDay(_due!),
                    firstDate: DateTime.now().subtract(
                      const Duration(days: 365),
                    ),
                    lastDate: DateTime.now().add(const Duration(days: 730)),
                  );
                  if (d != null) {
                    setState(
                      () => _due = fmtDay(DateTime.utc(d.year, d.month, d.day)),
                    );
                  }
                },
                icon: const Icon(Icons.event),
                label: Text(_due == null ? 'Set due date' : 'Due $_due'),
              ),
            ],
            const SizedBox(height: 16),
            Row(
              children: [
                Expanded(
                  child: Text(
                    _time == null
                        ? 'No reminder'
                        : 'Reminder at ${fmtTime(_time!)}',
                  ),
                ),
                TextButton(
                  onPressed: () async {
                    final init = _time == null
                        ? const TimeOfDay(hour: 17, minute: 0)
                        : TimeOfDay(
                            hour: int.parse(_time!.split(':')[0]),
                            minute: int.parse(_time!.split(':')[1]),
                          );
                    final t = await showTimePicker(
                      context: context,
                      initialTime: init,
                    );
                    if (t != null) {
                      setState(
                        () => _time =
                            '${t.hour.toString().padLeft(2, '0')}:${t.minute.toString().padLeft(2, '0')}',
                      );
                    }
                  },
                  child: Text(_time == null ? 'Set time' : 'Change'),
                ),
                if (_time != null)
                  TextButton(
                    onPressed: () => setState(() => _time = null),
                    child: const Text('Clear'),
                  ),
              ],
            ),
            const SizedBox(height: 8),
            TextField(
              controller: _points,
              keyboardType: TextInputType.number,
              decoration: const InputDecoration(labelText: 'XP when done'),
            ),
            if (editing)
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: const Text('Active'),
                value: _active,
                onChanged: (v) => setState(() => _active = v),
              ),
            if (_error != null)
              Padding(
                padding: const EdgeInsets.only(top: 8),
                child: Text(_error!, style: TextStyle(color: p.pink)),
              ),
            const SizedBox(height: 16),
            Row(
              children: [
                if (editing)
                  TextButton(
                    onPressed: () async {
                      final nav = Navigator.of(context);
                      await ref
                          .read(controllerProvider)
                          .deleteItem(widget.item!.id);
                      nav.pop();
                    },
                    child: Text('Delete', style: TextStyle(color: p.pink)),
                  ),
                const Spacer(),
                TextButton(
                  onPressed: () => Navigator.pop(context),
                  child: const Text('Cancel'),
                ),
                const SizedBox(width: 8),
                FilledButton(onPressed: _save, child: const Text('Save')),
              ],
            ),
          ],
        ),
      ),
    );
  }
}
