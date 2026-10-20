import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/api.dart';
import '../data/sync.dart';
import '../state.dart';
import '../theme.dart';
import 'items.dart';
import 'pair.dart';
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

    void toast(String m) => ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(m)));

    final statusText = switch (status) {
      SyncStatus.syncing => 'Syncing...',
      SyncStatus.offline => 'Cannot reach the server. Changes are saved here and will sync later.',
      SyncStatus.unpaired => paired ? 'This phone is no longer paired. Pair it again.' : 'Not paired',
      SyncStatus.idle => paired ? 'Up to date' : 'Not paired',
    };

    return SafeArea(
      bottom: false,
      child: ListView(
        padding: const EdgeInsets.fromLTRB(16, 20, 16, 24),
        children: [
          const Text('Settings', style: TextStyle(fontSize: 22, fontWeight: FontWeight.w700)),
          const SectionHeader('Sync'),
          Panel(
            child: Row(children: [
              StatusDot(ok: paired && status == SyncStatus.idle),
              const SizedBox(width: 12),
              Expanded(
                child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                  Text(syncLabel(store, paired)),
                  Text(statusText, style: TextStyle(color: p.dim, fontSize: 12)),
                ]),
              ),
            ]),
          ),
          const Spacer8(),
          if (paired)
            PanelButton(icon: Icons.sync_rounded, title: 'Sync now', onTap: () async {
              await ctl.syncNow();
              toast('Sync finished');
            }),
          if (paired) const Spacer8(),
          PanelButton(
            icon: Icons.link_rounded,
            title: paired ? 'Pair again' : 'Pair with server',
            subtitle: paired ? 'Use a new code, for example after reinstalling the server' : 'Enter a pairing code from your server',
            onTap: () => Navigator.of(context).push(MaterialPageRoute<void>(builder: (_) => const PairScreen())),
          ),
          const SectionHeader('Your list'),
          PanelButton(icon: Icons.tune_rounded, title: 'Manage items', subtitle: 'Chores, habits, to-dos, days and reminder times', onTap: () => Navigator.of(context).push(MaterialPageRoute<void>(builder: (_) => const ItemsScreen()))),
          const Spacer8(),
          PanelButton(
            icon: Icons.playlist_add_rounded,
            title: 'Add starter chores',
            subtitle: 'The weekly household schedule, skipping any already added',
            onTap: () async => toast('Added ${await ctl.addStarterChores()} chores'),
          ),
          const SectionHeader('Feedback'),
          Panel(
            padding: const EdgeInsets.symmetric(horizontal: 12),
            child: Material(
              color: Colors.transparent,
              child: Column(children: [
              StatefulBuilder(
                builder: (_, set) => SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: const Text('Sound effects'),
                  value: sfx.sound,
                  onChanged: (v) async {
                    sfx.sound = v;
                    await store.setSound(v);
                    set(() {});
                  },
                ),
              ),
              StatefulBuilder(
                builder: (_, set) => SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: const Text('Vibration'),
                  value: sfx.haptics,
                  onChanged: (v) async {
                    sfx.haptics = v;
                    await store.setHaptics(v);
                    set(() {});
                  },
                ),
              ),
            ]),
            ),
          ),
          const SectionHeader('Notifications'),
          PanelButton(
            icon: Icons.notifications_active_outlined,
            title: 'Send a test notification',
            subtitle: 'Checks that the server can reach this phone',
            onTap: () async {
              final token = await store.token;
              if (token == null) return toast('Pair with the server first.');
              try {
                await Api(store.serverUrl, token: token).pushTest();
                toast('Sent. It should arrive in a moment.');
              } on ApiException catch (e) {
                toast(e.message);
              } on AuthLost catch (e) {
                toast(e.toString());
              }
            },
          ),
        ],
      ),
    );
  }
}
