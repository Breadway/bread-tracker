import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/api.dart';
import '../data/sync.dart';
import '../push.dart';
import '../state.dart';
import '../theme.dart';
import 'items.dart';
import 'pair.dart';
import 'server_settings.dart';
import 'widgets.dart';

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

    final statusText = switch (status) {
      SyncStatus.syncing => 'Syncing...',
      SyncStatus.offline =>
        'Cannot reach the server. Changes are saved here and will sync later.',
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
            child: Row(
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
            title: paired ? 'Pair again' : 'Pair with server',
            subtitle: paired
                ? 'Use a new code, for example after reinstalling the server'
                : 'Enter a pairing code from your server',
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
                final revoked = await ctl.signOut(
                  wipeLocal: choice == _SignOut.wipe,
                );
                toast(
                  revoked
                      ? 'Signed out'
                      : 'Signed out here, but the server could not be reached. '
                            'Revoke this device from the server to be sure.',
                );
              },
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
                  ref.invalidate(pairedProvider);
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
