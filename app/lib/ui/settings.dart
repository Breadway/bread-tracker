import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart' show Clipboard, ClipboardData;
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:share_plus/share_plus.dart';

import '../data/api.dart';
import '../data/sync.dart';
import '../push.dart';
import '../state.dart';
import '../theme.dart';
import 'items.dart';
import 'pair.dart';
import 'server_settings.dart';
import 'widgets.dart';

final exportShareProvider = Provider<Future<void> Function(String)>(
  (ref) => (json) async {
    await SharePlus.instance.share(
      ShareParams(
        files: [
          XFile.fromData(
            Uint8List.fromList(utf8.encode(json)),
            mimeType: 'application/json',
            name: 'bread-tracker-export.json',
          ),
        ],
        fileNameOverrides: ['bread-tracker-export.json'],
        title: 'Bread Tracker export',
      ),
    );
  },
);

class SettingsScreen extends ConsumerWidget {
  const SettingsScreen({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final p = palOf(context);
    final store = ref.watch(storeProvider);
    final paired = ref.watch(pairedProvider).value ?? false;
    final status = ref.watch(syncStatusProvider);
    final sfx = ref.watch(sfxProvider);
    final ctl = ref.read(controllerProvider);

    void toast(String m) {
      if (context.mounted) {
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(m)));
      }
    }

    final rejection = ref.read(syncEngineProvider).rejectionMessage;
    String policy(String recovery) => rejection == null || rejection.isEmpty
        ? recovery
        : '$recovery $rejection';
    final statusText = switch (status) {
      SyncStatus.syncing => 'Syncing...',
      SyncStatus.offline =>
        'Cannot reach the server. Changes are saved here and will sync later.',
      SyncStatus.quotaExhausted => policy(
        'Sync paused: this account reached a server limit. Remove older items or history, '
        'or ask the server owner to raise the limit, then retry.',
      ),
      SyncStatus.accountDisabled => policy(
        'Sync paused: this account is disabled on the server. Ask the server owner to '
        're-enable it, then retry.',
      ),
      SyncStatus.rateLimited => policy(
        'Sync paused: the server is rate limiting this device. Wait a minute, then retry.',
      ),
      SyncStatus.rejected =>
        rejection ?? 'The server refused this sync. Local changes are kept.',
      SyncStatus.unpaired =>
        paired
            ? 'This phone is no longer paired. Pair it again.'
            : 'Not paired',
      SyncStatus.idle => paired ? 'Up to date' : 'Not paired',
    };

    return SafeArea(
      bottom: false,
      child: ListView(
        padding: const EdgeInsets.fromLTRB(16, 20, 16, 24),
        children: [
          const Text(
            'Settings',
            style: TextStyle(fontSize: 22, fontWeight: FontWeight.w700),
          ),
          const SectionHeader('Sync'),
          Panel(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  children: [
                    StatusDot(ok: paired && status == SyncStatus.idle),
                    const SizedBox(width: 12),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(syncLabel(store, paired)),
                          Text(
                            statusText,
                            style: TextStyle(color: p.dim, fontSize: 12),
                          ),
                        ],
                      ),
                    ),
                  ],
                ),
                if (paired && status.isPolicyRejection)
                  Align(
                    alignment: Alignment.centerRight,
                    child: TextButton(
                      onPressed: () async {
                        await ctl.syncNow();
                        toast('Sync retried');
                      },
                      child: const Text('Retry sync'),
                    ),
                  ),
              ],
            ),
          ),
          const Spacer8(),
          if (paired)
            PanelButton(
              icon: Icons.sync_rounded,
              title: 'Sync now',
              onTap: () async {
                await ctl.syncNow();
                toast('Sync finished');
              },
            ),
          if (paired) const Spacer8(),
          PanelButton(
            icon: Icons.link_rounded,
            title: paired ? 'Switch account or pair again' : 'Join or pair',
            subtitle: paired
                ? 'Join with an invite or use a code. If it is a different account, this phone\'s data is replaced'
                : 'Join with an invite code, or pair with a code',
            onTap: () => Navigator.of(
              context,
            ).push(MaterialPageRoute<void>(builder: (_) => const PairScreen())),
          ),
          if (paired) const Spacer8(),
          if (paired)
            PanelButton(
              icon: Icons.logout_rounded,
              title: 'Sign out',
              subtitle: 'Stop syncing and reminders on this phone',
              onTap: () async {
                final choice = await showDialog<_SignOut>(
                  context: context,
                  builder: (_) => const _SignOutDialog(),
                );
                if (choice == null) return;
                var ownerGeneration = ctl.currentAuthGeneration;
                var ownerStoreGeneration = store.authGeneration;
                final ownerServer = store.serverUrl;
                Future<void> finishSignOut() async {
                  if (ctl.currentAuthGeneration != ownerGeneration ||
                      store.authGeneration != ownerStoreGeneration ||
                      store.serverUrl != ownerServer) {
                    return;
                  }
                  try {
                    final pending = ctl.signOut(
                      wipeLocal: choice == _SignOut.wipe,
                      expectedAuthGeneration: ownerGeneration,
                      expectedStoreGeneration: ownerStoreGeneration,
                      expectedServer: ownerServer,
                    );
                    // signOut claims ownership synchronously before its first
                    // await. Retain that attempt's identity, not the identity
                    // that happens to exist when a later failure is handled.
                    ownerGeneration = ctl.currentAuthGeneration;
                    ownerStoreGeneration = store.authGeneration;
                    final revoked = await pending;
                    toast(
                      revoked
                          ? 'Signed out'
                          : 'Signed out here, but the server could not be reached. '
                                'Revoke this device from the server to be sure.',
                    );
                  } catch (_) {
                    if (!context.mounted) return;
                    if (ctl.currentAuthGeneration != ownerGeneration ||
                        store.authGeneration != ownerStoreGeneration ||
                        store.serverUrl != ownerServer) {
                      return;
                    }
                    ScaffoldMessenger.of(context).showSnackBar(
                      SnackBar(
                        content: const Text(
                          'Sign-out cleanup failed on this phone. '
                          'Sync is paused. Retry local cleanup.',
                        ),
                        action: SnackBarAction(
                          label: 'Retry',
                          onPressed: finishSignOut,
                        ),
                      ),
                    );
                  }
                }

                await finishSignOut();
              },
            ),
          if (paired) const Spacer8(),
          if (paired)
            PanelButton(
              icon: Icons.download_rounded,
              title: 'Export my data',
              subtitle: 'Copy everything stored for your account as JSON',
              onTap: () async {
                try {
                  final json = await ctl.exportJson();
                  if (json.length > _maxClipboardChars) {
                    await ref.read(exportShareProvider)(json);
                    return;
                  }
                  await Clipboard.setData(ClipboardData(text: json));
                  toast(
                    'Copied ${json.length ~/ 1024 + 1} KB of JSON to the clipboard',
                  );
                } on ApiException catch (e) {
                  toast(e.message);
                } on AuthLost catch (e) {
                  toast(e.toString());
                } catch (_) {
                  toast(
                    'Could not save or share the export. Your data is unchanged. Retry export.',
                  );
                }
              },
            ),
          if (paired) const Spacer8(),
          if (paired)
            PanelButton(
              icon: Icons.delete_forever_rounded,
              title: 'Delete my account',
              subtitle: 'Permanently remove your account and all of your data from the server',
              onTap: () async {
                final name = store.account?.name ?? '';
                final typed = await showDialog<String>(
                  context: context,
                  builder: (_) => _DeleteAccountDialog(name: name),
                );
                if (typed == null) return;
                try {
                  await ctl.deleteAccount(typed);
                  toast('Your account was deleted');
                } on ApiException catch (e) {
                  toast(e.message);
                } on AuthLost catch (e) {
                  toast(e.toString());
                } catch (_) {
                  toast(
                    'Account cleanup failed on this phone. Local sync is paused; retry cleanup before pairing.',
                  );
                }
              },
            ),
          if (paired) const Spacer8(),
          if (paired)
            PanelButton(
              icon: Icons.devices_rounded,
              title: 'Add another device',
              subtitle: 'Create a short-lived code for your own account',
              onTap: () => showDialog<void>(
                context: context,
                builder: (_) => const _PairCodeDialog(),
              ),
            ),
          const SectionHeader('Your list'),
          PanelButton(
            icon: Icons.tune_rounded,
            title: 'Manage items',
            subtitle: 'Chores, habits, to-dos, days and reminder times',
            onTap: () => Navigator.of(context).push(
              MaterialPageRoute<void>(builder: (_) => const ItemsScreen()),
            ),
          ),
          const Spacer8(),
          PanelButton(
            icon: Icons.playlist_add_rounded,
            title: 'Add example chores',
            subtitle: 'A few generic weekly chores, skipping any already added',
            onTap: () async =>
                toast('Added ${await ctl.addStarterChores()} chores'),
          ),
          const SectionHeader('Feedback'),
          Panel(
            padding: const EdgeInsets.symmetric(horizontal: 12),
            child: Material(
              color: Colors.transparent,
              child: Column(
                children: [
                  StatefulBuilder(
                    builder: (_, set) => SwitchListTile(
                      contentPadding: EdgeInsets.zero,
                      title: const Text('Sound effects'),
                      value: sfx.sound,
                      onChanged: (v) async {
                        try {
                          await store.setSound(v);
                          sfx.sound = v;
                          set(() {});
                        } catch (error) {
                          if (!context.mounted) return;
                          ScaffoldMessenger.of(context).showSnackBar(
                            SnackBar(
                              content: Text(
                                'Could not save sound preference: $error',
                              ),
                            ),
                          );
                        }
                      },
                    ),
                  ),
                  StatefulBuilder(
                    builder: (_, set) => SwitchListTile(
                      contentPadding: EdgeInsets.zero,
                      title: const Text('Vibration'),
                      value: sfx.haptics,
                      onChanged: (v) async {
                        try {
                          await store.setHaptics(v);
                          sfx.haptics = v;
                          set(() {});
                        } catch (error) {
                          if (!context.mounted) return;
                          ScaffoldMessenger.of(context).showSnackBar(
                            SnackBar(
                              content: Text(
                                'Could not save vibration preference: $error',
                              ),
                            ),
                          );
                        }
                      },
                    ),
                  ),
                ],
              ),
            ),
          ),
          if (paired) const SectionHeader('Reminders'),
          if (paired) const ServerSettingsSection(),
          const SectionHeader('Notifications'),
          ValueListenableBuilder<PushStatus>(
            valueListenable: ref.watch(pushProvider).status,
            builder: (context, status, _) => PanelButton(
              icon: Icons.notifications_outlined,
              title: switch (status) {
                PushStatus.ready => 'This phone is registered',
                PushStatus.denied => 'Notifications are not allowed',
                PushStatus.unpaired => 'Pair to enable reminders',
                PushStatus.registering => 'Registering notifications...',
                PushStatus.failed => 'Notification registration failed',
                PushStatus.unavailable => 'Notifications are unavailable',
              },
              subtitle: 'Tap to enable or retry. If blocked, allow notifications in Android app settings.',
              onTap: () =>
                  ref.read(pushProvider).register(requestPermission: true),
            ),
          ),
          const Spacer8(),
          PanelButton(
            icon: Icons.notifications_active_outlined,
            title: 'Send a test notification',
            subtitle: 'Checks that the server can reach this phone',
            onTap: () async {
              final generation = store.authGeneration;
              final server = store.serverUrl;
              final token = await store.readTokenForGeneration(generation);
              if (token == null ||
                  store.authGeneration != generation ||
                  store.serverUrl != server) {
                return toast('Pair with the server first.');
              }
              try {
                final push = ref.read(pushProvider);
                await push.register(requestPermission: true);
                if (push.status.value != PushStatus.ready) {
                  toast('Enable notifications and register this phone first.');
                  return;
                }
                await ref.read(apiFactoryProvider)(server, token).pushTest();
                toast('Sent. It should arrive in a moment.');
              } on ApiException catch (e) {
                toast(e.message);
              } on AuthLost catch (e) {
                if (await store.clearTokenIfCurrentForGeneration(
                  token,
                  generation,
                  expectedServer: server,
                )) {
                  ref.read(pairedStateRevisionProvider.notifier).refresh();
                  await ref.read(pushProvider).register();
                }
                toast(e.toString());
              }
            },
          ),
        ],
      ),
    );
  }
}

enum _SignOut { keep, wipe }

class _PairCodeDialog extends ConsumerStatefulWidget {
  const _PairCodeDialog();

  @override
  ConsumerState<_PairCodeDialog> createState() => _PairCodeDialogState();
}

class _PairCodeDialogState extends ConsumerState<_PairCodeDialog> {
  String? _code;
  DateTime? _expiresAt;
  String? _error;
  bool _busy = false;
  int? _ownerGeneration;
  int? _ownerAppGeneration;
  String? _ownerServer;

  @override
  void initState() {
    super.initState();
    Future.microtask(_load);
  }

  Future<void> _load() async {
    if (!mounted || _busy) return;
    final store = ref.read(storeProvider);
    final controller = ref.read(controllerProvider);
    final generation = store.authGeneration;
    final appGeneration = controller.currentAuthGeneration;
    final server = store.serverUrl;
    bool ownsRequest() =>
        mounted &&
        store.authGeneration == generation &&
        controller.currentAuthGeneration == appGeneration &&
        store.serverUrl == server;
    setState(() {
      _busy = true;
      _code = null;
      _error = null;
      _ownerGeneration = generation;
      _ownerAppGeneration = appGeneration;
      _ownerServer = server;
    });
    try {
      final token = await store.readTokenForGeneration(generation);
      if (!ownsRequest()) return;
      if (token == null) throw ApiException('Pair this phone first.');
      final result = await ref
          .read(apiFactoryProvider)(server, token)
          .createPairCode();
      if (!ownsRequest()) return;
      final code = result['code'];
      final ttl = result['expires_in_secs'];
      if (code is! String || code.isEmpty || ttl is! int || ttl <= 0) {
        throw ApiException('Invalid pairing-code response.');
      }
      setState(() {
        _code = code;
        _expiresAt = DateTime.now().add(Duration(seconds: ttl));
      });
    } catch (error) {
      if (ownsRequest()) setState(() => _error = error.toString());
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    // A code is account-sensitive: hide it as soon as the owning session changes.
    ref.watch(pairedStateRevisionProvider);
    final paired = ref.watch(pairedProvider).value ?? false;
    final store = ref.watch(storeProvider);
    final sameOwner =
        store.authGeneration == _ownerGeneration &&
        ref.read(controllerProvider).currentAuthGeneration ==
            _ownerAppGeneration &&
        store.serverUrl == _ownerServer;
    return AlertDialog(
      title: const Text('Add another device'),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (!paired)
            const Text('Pair this phone first.')
          else if (_busy)
            const CircularProgressIndicator()
          else if (_error != null)
            Text(_error!)
          else if (_code != null && sameOwner) ...[
            SelectableText(_code!),
            Text('Expires at ${_expiresAt!.toLocal()}. Single use.'),
          ] else if (!sameOwner)
            const Text('Account changed. Request a new code.'),
        ],
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('Close'),
        ),
        if (paired)
          FilledButton(
            onPressed: _busy ? null : _load,
            child: const Text('New code'),
          ),
      ],
    );
  }
}

/// Android's binder limit is about 1 MB per transaction; stay well under it.
const _maxClipboardChars = 500 * 1000;

/// Asks the user to type the account name before deleting. Owns its controller.
class _DeleteAccountDialog extends StatefulWidget {
  const _DeleteAccountDialog({required this.name});
  final String name;
  @override
  State<_DeleteAccountDialog> createState() => _DeleteAccountDialogState();
}

class _DeleteAccountDialogState extends State<_DeleteAccountDialog> {
  final _c = TextEditingController();

  @override
  void dispose() {
    _c.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final matches =
        widget.name.isNotEmpty &&
        _c.text.trim().toLowerCase() == widget.name.toLowerCase();
    return AlertDialog(
      title: const Text('Delete your account?'),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Text(
            'This removes your account, items, history and settings from the server and '
            'from this phone, and signs out every device on the account. It cannot be '
            'undone. Export your data first if you want a copy.',
          ),
          const SizedBox(height: 12),
          Text('Type "${widget.name}" to confirm.'),
          TextField(
            controller: _c,
            autofocus: true,
            onChanged: (_) => setState(() {}),
            decoration: InputDecoration(hintText: widget.name),
          ),
        ],
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: matches
              ? () => Navigator.pop(context, _c.text.trim())
              : null,
          child: const Text('Delete everything'),
        ),
      ],
    );
  }
}

class _SignOutDialog extends StatelessWidget {
  const _SignOutDialog();

  @override
  Widget build(BuildContext context) => AlertDialog(
    title: const Text('Sign out of this phone?'),
    content: const Text(
      'This phone stops syncing and its reminders stop. '
      'You can keep the items and ticks stored on this phone, or delete them. '
      'Anything not yet synced is lost if you delete.',
    ),
    actions: [
      TextButton(
        onPressed: () => Navigator.pop(context),
        child: const Text('Cancel'),
      ),
      TextButton(
        onPressed: () => Navigator.pop(context, _SignOut.wipe),
        child: const Text('Sign out and delete data'),
      ),
      FilledButton(
        onPressed: () => Navigator.pop(context, _SignOut.keep),
        child: const Text('Sign out'),
      ),
    ],
  );
}
