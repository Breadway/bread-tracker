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

enum _Mode { join, pair }

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
  final _name = TextEditingController();
  _Mode _mode = _Mode.join;
  bool _busy = false;
  String? _error;

  @override
  void dispose() {
    _server.dispose();
    _code.dispose();
    _name.dispose();
    super.dispose();
  }

  Future<bool> _confirmReplace() async {
    final ctl = ref.read(controllerProvider);
    if (!await ctl.localDataAtRisk()) return true;
    if (!mounted) return false;
    final join = _mode == _Mode.join;
    final ok = await showDialog<bool>(
      context: context,
      builder: (_) => AlertDialog(
        title: const Text('Replace the data on this phone?'),
        content: Text(
          join
              ? 'This phone already holds items and ticks from another account. '
                    'Joining a new account deletes them from this phone first, so they '
                    'never mix into the new account. Anything not yet synced to the old '
                    'account is lost.'
              : 'This phone already holds items and ticks. If this code belongs to a '
                    'different account than the one they came from, they are deleted '
                    'from this phone first so they never mix into the other account. '
                    'Anything not yet synced to the old account is lost.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Continue'),
          ),
        ],
      ),
    );
    return ok ?? false;
  }

  Future<void> _submit() async {
    final server = _server.text.trim().replaceAll(RegExp(r'/+$'), '');
    if (!server.startsWith('https://') && !server.startsWith('http://')) {
      return setState(
        () => _error = 'The server address must start with https://',
      );
    }
    final join = _mode == _Mode.join;
    final name = _name.text.trim();
    if (join && name.isEmpty) {
      return setState(() => _error = 'Choose a name for your account.');
    }
    if (_code.text.replaceAll(RegExp(r'[^A-Za-z0-9]'), '').length < 8) {
      return setState(
        () => _error = join
            ? 'Enter the invite code you were given.'
            : 'Enter the 8 character pairing code.',
      );
    }
    if (!await _confirmReplace() || !mounted) return;
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final api = ref.read(apiFactoryProvider)(server, null);
      final AccountInfo account;
      if (join) {
        account = await api.signup(
          invite: _code.text.trim(),
          name: name,
          deviceName: 'Android',
          timezone: await ref.read(phoneTimezoneProvider)(),
        );
      } else {
        account = await api.pair(_code.text.trim(), 'Android');
      }
      await ref.read(controllerProvider).adoptAccount(
        server: server,
        token: account.token!,
        account: account,
      );
      if (mounted && Navigator.canPop(context)) Navigator.pop(context);
    } on ApiException catch (e) {
      setState(() => _error = _friendly(e.message, join));
    } on AuthLost catch (e) {
      setState(() => _error = e.toString());
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  String _friendly(String message, bool join) {
    if (message == 'unauthorized') {
      return join
          ? 'That invite code is wrong, already used, or has expired. Ask for a new one.'
          : 'That code is wrong or has expired. Make a new one.';
    }
    if (message.contains('already taken')) {
      return 'That name is already taken. Choose another.';
    }
    if (message.contains('too many')) {
      return 'Too many wrong attempts. Wait a few minutes and try again.';
    }
    return message;
  }

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final join = _mode == _Mode.join;
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
              join
                  ? 'Join with the invite code you were given to get your own account. '
                        'Your chores, habits and reminders are private to you. The person who '
                        'runs the server can see the data stored on it.'
                  : 'Add this phone to an account you already have, so your chores, habits '
                        'and reminders stay in sync.',
              style: TextStyle(color: p.dim, height: 1.4),
            ),
            const SizedBox(height: 16),
            SegmentedButton<_Mode>(
              segments: const [
                ButtonSegment(value: _Mode.join, label: Text('Join with an invite')),
                ButtonSegment(value: _Mode.pair, label: Text('Pair with a code')),
              ],
              selected: {_mode},
              onSelectionChanged: _busy
                  ? null
                  : (s) => setState(() {
                      _mode = s.first;
                      _error = null;
                    }),
            ),
            const SizedBox(height: 16),
            TextField(
              controller: _server,
              keyboardType: TextInputType.url,
              decoration: const InputDecoration(labelText: 'Server'),
            ),
            const SizedBox(height: 12),
            if (join) ...[
              TextField(
                controller: _name,
                textCapitalization: TextCapitalization.words,
                decoration: const InputDecoration(
                  labelText: 'Your name',
                  helperText: 'Letters, digits, spaces, . _ - (up to 40)',
                ),
              ),
              const SizedBox(height: 12),
            ],
            TextField(
              controller: _code,
              textCapitalization: TextCapitalization.characters,
              decoration: InputDecoration(
                labelText: join ? 'Invite code' : 'Pairing code',
                hintText: join ? 'XXXX-XXXX-XXXX' : 'XXXX-XXXX',
              ),
            ),
            const SizedBox(height: 8),
            Text(
              join
                  ? 'Invite codes come from whoever runs the server. Each works once.'
                  : 'Get a code from Settings on a phone that is already signed in to the '
                        'account (Add another device), or ask whoever runs the server.',
              style: TextStyle(color: p.dim, fontSize: 12),
            ),
            if (_error != null)
              Padding(
                padding: const EdgeInsets.only(top: 12),
                child: Text(_error!, style: TextStyle(color: p.pink)),
              ),
            const SizedBox(height: 20),
            FilledButton(
              onPressed: _busy ? null : _submit,
              child: Text(
                _busy ? (join ? 'Joining...' : 'Pairing...') : (join ? 'Join' : 'Pair'),
              ),
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

String syncLabel(Store store, bool paired) {
  if (!paired) return 'Not paired (data stays on this phone)';
  final name = store.account?.name ?? '';
  return name.isEmpty
      ? 'Paired with ${store.serverUrl}'
      : 'Signed in as $name on ${store.serverUrl}';
}

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
