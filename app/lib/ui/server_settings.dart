import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/api.dart';
import '../state.dart';
import '../theme.dart';
import 'pair.dart' show PanelButton, Spacer8;
import 'widgets.dart';

/// Loads the server's reminder settings. Re-read by invalidating after a change.
///
/// No automatic retry: the section shows the error with a Retry button instead of
/// hammering an unreachable server in the background.
final serverSettingsProvider = FutureProvider.autoDispose<ServerSettings>((
  ref,
) async {
  final store = ref.watch(storeProvider);
  final token = await store.token;
  if (token == null) throw ApiException('Not paired');
  return ref.watch(apiFactoryProvider)(store.serverUrl, token).getSettings();
}, retry: (_, _) => null);

String _hhmm(TimeOfDay t) =>
    '${t.hour.toString().padLeft(2, '0')}:${t.minute.toString().padLeft(2, '0')}';

TimeOfDay? _parse(String s) {
  final m = RegExp(r'^(\d{1,2}):(\d{2})$').firstMatch(s);
  if (m == null) return null;
  return TimeOfDay(hour: int.parse(m[1]!), minute: int.parse(m[2]!));
}

String _offsetLabel(int minutes) {
  final sign = minutes < 0 ? '-' : '+';
  final a = minutes.abs();
  return 'UTC$sign${(a ~/ 60).toString().padLeft(2, '0')}:${(a % 60).toString().padLeft(2, '0')}';
}

/// Reminder settings that live on the server: reminders on/off, quiet hours, timezone.
/// Shown only when the phone is paired.
class ServerSettingsSection extends ConsumerWidget {
  const ServerSettingsSection({super.key, this.phoneOffset});

  /// The phone's current UTC offset. Injectable for tests.
  final Duration? phoneOffset;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final p = palOf(context);
    final settings = ref.watch(serverSettingsProvider);

    return settings.when(
      loading: () => const Panel(child: Text('Loading reminder settings...')),
      error: (e, _) => Panel(
        child: Row(
          children: [
            Expanded(
              child: Text(
                e is ApiException
                    ? 'Could not load reminder settings: ${e.message}'
                    : 'Could not load reminder settings.',
                style: TextStyle(color: p.dim),
              ),
            ),
            TextButton(
              onPressed: () => ref.invalidate(serverSettingsProvider),
              child: const Text('Retry'),
            ),
          ],
        ),
      ),
      data: (s) => _Loaded(settings: s, phoneOffset: phoneOffset),
    );
  }
}

class _Loaded extends ConsumerWidget {
  const _Loaded({required this.settings, this.phoneOffset});
  final ServerSettings settings;
  final Duration? phoneOffset;

  Future<void> _save(
    BuildContext context,
    WidgetRef ref,
    Map<String, Object?> patch,
  ) async {
    final messenger = ScaffoldMessenger.of(context);
    try {
      final store = ref.read(storeProvider);
      final token = await store.token;
      if (token == null) throw ApiException('Not paired');
      await ref.read(apiFactoryProvider)(store.serverUrl, token).putSettings(patch);
      ref.invalidate(serverSettingsProvider);
    } on ApiException catch (e) {
      messenger.showSnackBar(SnackBar(content: Text(e.message)));
    } on AuthLost catch (e) {
      messenger.showSnackBar(SnackBar(content: Text(e.toString())));
    } catch (_) {
      messenger.showSnackBar(
        const SnackBar(content: Text('Could not save. Check the connection.')),
      );
    }
  }

  Future<void> _pickQuiet(BuildContext context, WidgetRef ref, bool start) async {
    final cur = _parse(start ? settings.quietStart : settings.quietEnd) ??
        TimeOfDay(hour: start ? 22 : 7, minute: 0);
    final picked = await showTimePicker(context: context, initialTime: cur);
    if (picked == null || !context.mounted) return;
    // The server needs both ends or neither, so fill the other end with a sensible default.
    final other = start ? settings.quietEnd : settings.quietStart;
    final otherValue = other.isNotEmpty ? other : (start ? '07:00' : '22:00');
    await _save(context, ref, {
      if (start) 'quiet_start': _hhmm(picked) else 'quiet_end': _hhmm(picked),
      if (start) 'quiet_end': otherValue else 'quiet_start': otherValue,
    });
  }

  Future<void> _editZone(BuildContext context, WidgetRef ref) async {
    final value = await showDialog<String>(
      context: context,
      builder: (_) => _ZoneDialog(initial: settings.timezone),
    );
    if (value == null || value.isEmpty || !context.mounted) return;
    await _save(context, ref, {'timezone': value});
  }

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final p = palOf(context);
    final offset = phoneOffset ?? DateTime.now().timeZoneOffset;
    final differs = settings.zoneDiffersFrom(offset);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Panel(
          padding: const EdgeInsets.symmetric(horizontal: 12),
          child: Material(
            color: Colors.transparent,
            child: SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('Send reminders'),
              subtitle: const Text('Turn off to stop all reminder notifications'),
              value: settings.remindersEnabled,
              onChanged: (v) => _save(context, ref, {'reminders_enabled': v}),
            ),
          ),
        ),
        const Spacer8(),
        PanelButton(
          icon: Icons.bedtime_outlined,
          title: settings.hasQuietHours
              ? 'Quiet hours ${settings.quietStart} to ${settings.quietEnd}'
              : 'Quiet hours are off',
          subtitle: settings.hasQuietHours
              ? 'Tap to change the start. Reminders wait until they end.'
              : 'Tap to choose when reminders should stay silent',
          onTap: () => _pickQuiet(context, ref, true),
        ),
        if (settings.hasQuietHours) ...[
          const Spacer8(),
          PanelButton(
            icon: Icons.wb_twilight_rounded,
            title: 'Quiet hours end at ${settings.quietEnd}',
            subtitle: 'Tap to change the end',
            onTap: () => _pickQuiet(context, ref, false),
          ),
          const Spacer8(),
          PanelButton(
            icon: Icons.notifications_active_outlined,
            title: 'Turn quiet hours off',
            onTap: () => _save(context, ref, {'quiet_start': '', 'quiet_end': ''}),
          ),
        ],
        const Spacer8(),
        PanelButton(
          icon: Icons.public_rounded,
          title: 'Server timezone: ${settings.timezone}',
          subtitle: settings.utcOffsetMinutes == null
              ? 'Tap to change'
              : '${_offsetLabel(settings.utcOffsetMinutes!)} now. Tap to change.',
          onTap: () => _editZone(context, ref),
        ),
        if (differs)
          Padding(
            padding: const EdgeInsets.fromLTRB(4, 8, 4, 0),
            child: Text(
              'This phone is on ${_offsetLabel(offset.inMinutes)} but the server uses '
              '${_offsetLabel(settings.utcOffsetMinutes!)}. Reminder times, quiet hours '
              'and "done today" follow the server, so they can look off by '
              '${((settings.utcOffsetMinutes! - offset.inMinutes).abs() / 60).toStringAsFixed(1)} hours. '
              'Set the server timezone to where you are.',
              style: TextStyle(color: p.dim, fontSize: 12),
            ),
          ),
      ],
    );
  }
}

/// Owns its text controller so it is disposed with the dialog, after the close animation.
class _ZoneDialog extends StatefulWidget {
  const _ZoneDialog({required this.initial});
  final String initial;
  @override
  State<_ZoneDialog> createState() => _ZoneDialogState();
}

class _ZoneDialogState extends State<_ZoneDialog> {
  late final TextEditingController _c = TextEditingController(
    text: widget.initial,
  );

  @override
  void dispose() {
    _c.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => AlertDialog(
    title: const Text('Server timezone'),
    content: TextField(
      controller: _c,
      autofocus: true,
      decoration: const InputDecoration(
        hintText: 'For example Australia/Perth',
        helperText: 'Reminder times and "today" follow this zone.',
      ),
    ),
    actions: [
      TextButton(
        onPressed: () => Navigator.pop(context),
        child: const Text('Cancel'),
      ),
      FilledButton(
        onPressed: () => Navigator.pop(context, _c.text.trim()),
        child: const Text('Save'),
      ),
    ],
  );
}
