import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/api.dart';
import '../data/store.dart';
import '../state.dart';
import '../theme.dart';
import 'widgets.dart';

/// Whether the device holds a token. Invalidate after pairing or unpairing.
final pairedProvider = FutureProvider<bool>(
  (ref) async => (await ref.watch(storeProvider).token) != null,
);

class PairScreen extends ConsumerStatefulWidget {
  const PairScreen({super.key, this.onSkip});
  final VoidCallback? onSkip;
  @override
  ConsumerState<PairScreen> createState() => _PairScreenState();
}

class _PairScreenState extends ConsumerState<PairScreen> {
  late final TextEditingController _server = TextEditingController(
    text: ref.read(storeProvider).serverUrl,
  );
  final _code = TextEditingController();
  bool _busy = false;
  String? _error;

  @override
  void dispose() {
    _server.dispose();
    _code.dispose();
    super.dispose();
  }

  Future<void> _pair() async {
    final server = _server.text.trim().replaceAll(RegExp(r'/+$'), '');
    if (!server.startsWith('https://') && !server.startsWith('http://')) {
      return setState(
        () => _error = 'The server address must start with https://',
      );
    }
    if (_code.text.trim().length < 8) {
      return setState(() => _error = 'Enter the 8 character pairing code.');
    }
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final token = await Api(server).pair(_code.text.trim(), 'Android');
      // Claim the new auth generation before any credential/cursor write awaits,
      // so a pending sign-out cannot wipe data belonging to this pairing.
      await ref.read(controllerProvider).pairedAgain();
      final store = ref.read(storeProvider);
      await store.setServerUrl(server);
      await store.saveToken(token);
      await store.resetSync();
      ref.read(controllerProvider).pairingPersisted();
      ref.invalidate(pairedProvider);
      ref.read(controllerProvider).syncNow();
      ref.read(pushProvider).register(requestPermission: true);
      if (mounted && Navigator.canPop(context)) Navigator.pop(context);
    } on ApiException catch (e) {
      setState(
        () => _error = e.message == 'unauthorized'
            ? 'That code is wrong or has expired. Make a new one on the server.'
            : e.message,
      );
    } on AuthLost catch (e) {
      setState(() => _error = e.toString());
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return Scaffold(
      body: SafeArea(
        child: ListView(
          padding: const EdgeInsets.all(24),
          children: [
            const SizedBox(height: 24),
            Icon(Icons.bakery_dining_rounded, size: 48, color: p.accent),
            const SizedBox(height: 16),
            const Text(
              'Bread Tracker',
              style: TextStyle(fontSize: 26, fontWeight: FontWeight.w700),
            ),
            const SizedBox(height: 8),
            Text(
              'Pair this phone with your server so your chores, habits and reminders stay in sync.',
              style: TextStyle(color: p.dim, height: 1.4),
            ),
            const SizedBox(height: 24),
            TextField(
              controller: _server,
              keyboardType: TextInputType.url,
              decoration: const InputDecoration(labelText: 'Server'),
            ),
            const SizedBox(height: 12),
            TextField(
              controller: _code,
              textCapitalization: TextCapitalization.characters,
              decoration: const InputDecoration(
                labelText: 'Pairing code',
                hintText: 'XXXX-XXXX',
              ),
            ),
            const SizedBox(height: 8),
            Text(
              'Make a code on the server with: docker exec bread-tracker bread-tracker-server pair',
              style: TextStyle(color: p.dim, fontSize: 12),
            ),
            if (_error != null)
              Padding(
                padding: const EdgeInsets.only(top: 12),
                child: Text(_error!, style: TextStyle(color: p.pink)),
              ),
            const SizedBox(height: 20),
            FilledButton(
              onPressed: _busy ? null : _pair,
              child: Text(_busy ? 'Pairing...' : 'Pair'),
            ),
            if (widget.onSkip != null) ...[
              const SizedBox(height: 8),
              TextButton(
                onPressed: widget.onSkip,
                child: const Text('Use offline for now'),
              ),
            ],
          ],
        ),
      ),
    );
  }
}

String syncLabel(Store store, bool paired) => paired
    ? 'Paired with ${store.serverUrl}'
    : 'Not paired (data stays on this phone)';

/// Small status chip shown on the settings screen.
class StatusDot extends StatelessWidget {
  const StatusDot({super.key, required this.ok});
  final bool ok;
  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return Container(
      width: 10,
      height: 10,
      decoration: BoxDecoration(
        shape: BoxShape.circle,
        color: ok ? p.good : p.dim,
      ),
    );
  }
}

class Spacer8 extends StatelessWidget {
  const Spacer8({super.key});
  @override
  Widget build(BuildContext context) => const SizedBox(height: 8);
}

class PanelButton extends StatelessWidget {
  const PanelButton({
    super.key,
    required this.icon,
    required this.title,
    this.subtitle,
    required this.onTap,
    this.trailing,
  });
  final IconData icon;
  final String title;
  final String? subtitle;
  final VoidCallback onTap;
  final Widget? trailing;
  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return GestureDetector(
      onTap: onTap,
      behavior: HitTestBehavior.opaque,
      child: Panel(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 14),
        child: Row(
          children: [
            Icon(icon, color: p.accent, size: 22),
            const SizedBox(width: 12),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(title),
                  if (subtitle != null)
                    Text(
                      subtitle!,
                      style: TextStyle(color: p.dim, fontSize: 12),
                    ),
                ],
              ),
            ),
            trailing ?? Icon(Icons.chevron_right, color: p.dim),
          ],
        ),
      ),
    );
  }
}
