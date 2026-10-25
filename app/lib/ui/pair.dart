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
  String? _issuedTokenForRetry;
  String? _issuedServerForRetry;
  String? _issuedCodeForRetry;
  String? _claimedTokenForRetry;
  int _pairAttemptGeneration = 0;
  bool _disposed = false;

  @override
  void initState() {
    super.initState();
    _server.addListener(_onPairInputChanged);
    _code.addListener(_onPairInputChanged);
  }

  void _onPairInputChanged() {
    if (_issuedTokenForRetry != null || _claimedTokenForRetry != null) {
      _invalidatePendingPairingResult();
    } else {
      _pairAttemptGeneration++;
    }
  }

  void _invalidatePendingPairingResult() {
    _pairAttemptGeneration++;
    _issuedTokenForRetry = null;
    _issuedServerForRetry = null;
    _issuedCodeForRetry = null;
    _claimedTokenForRetry = null;
  }

  Future<void> _recoverKeepingData() async {
    final controller = ref.read(controllerProvider);
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final recovered = await controller.signOut(wipeLocal: false);
      if (!mounted) return;
      if (!recovered || controller.isWipeRecoveryPending) {
        setState(
          () => _error = 'Local recovery could not be verified. Your data and pairing result remain preserved. Try again.',
        );
        return;
      }
      setState(
        () => _error = 'Retained data is verified and kept. Tap Pair to finish using the already-issued pairing result.',
      );
    } catch (e) {
      if (mounted) {
        setState(
          () => _error =
              'Local recovery failed. Your data and pairing result remain preserved. Details: $e',
        );
      }
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  void dispose() {
    _disposed = true;
    _invalidatePendingPairingResult();
    _server.removeListener(_onPairInputChanged);
    _code.removeListener(_onPairInputChanged);
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
    final attemptServer = server;
    final attemptCode = _code.text.trim();
    final attemptGeneration = ++_pairAttemptGeneration;
    bool attemptStillCurrent() =>
        !_disposed &&
        mounted &&
        attemptGeneration == _pairAttemptGeneration &&
        _server.text.trim().replaceAll(RegExp(r'/+$'), '') == attemptServer &&
        _code.text.trim() == attemptCode;
    final controller = ref.read(controllerProvider);
    final store = ref.read(storeProvider);
    final retryingIssuedToken =
        _issuedTokenForRetry != null &&
        _issuedServerForRetry == attemptServer &&
        _issuedCodeForRetry == attemptCode;
    var codeWasRedeemed = false;
    String? issuedToken;
    try {
      String? token = retryingIssuedToken ? _issuedTokenForRetry : null;
      if (!retryingIssuedToken) {
        token ??= await Api(attemptServer).pair(attemptCode, 'Android');
        codeWasRedeemed = true;
      }
      final pairedToken = token!;
      issuedToken = pairedToken;
      if (!attemptStillCurrent()) {
        _invalidatePendingPairingResult();
        return;
      }
      // Claim the new auth generation before any credential/cursor write awaits,
      // so a pending sign-out cannot wipe data belonging to this pairing.
      try {
        if (retryingIssuedToken) {
          if (!await controller.claimPairingAfterRecovery()) {
            throw StateError(
              'Retained data recovery verification did not pass.',
            );
          }
        } else {
          await controller.pairedAgain();
        }
        if (retryingIssuedToken) _claimedTokenForRetry = pairedToken;
      } on Exception catch (e) {
        if (!attemptStillCurrent()) {
          _invalidatePendingPairingResult();
          return;
        }
        _issuedTokenForRetry = pairedToken;
        _issuedServerForRetry = attemptServer;
        _issuedCodeForRetry = attemptCode;
        setState(
          () => _error =
              'Local recovery is incomplete. Existing data was preserved. Retry pairing to recover without redeeming the one-time code again. Details: $e',
        );
        return;
      }
      if (!attemptStillCurrent()) {
        _invalidatePendingPairingResult();
        return;
      }
      await store.setServerUrl(attemptServer);
      await store.saveToken(pairedToken);
      await store.resetSync();
      if (!attemptStillCurrent()) {
        await controller.abandonRecoveredPairing(pairedToken);
        _invalidatePendingPairingResult();
        return;
      }
      controller.pairingPersisted();
      _invalidatePendingPairingResult();
      ref.invalidate(pairedProvider);
      controller.syncNow();
      ref.read(pushProvider).register(requestPermission: true);
      if (mounted && Navigator.canPop(context)) Navigator.pop(context);
    } on ApiException catch (e) {
      if (mounted) {
        setState(
          () => _error = e.message == 'unauthorized'
              ? 'That code is wrong or has expired. Make a new one on the server.'
              : e.message,
        );
      }
    } on AuthLost catch (e) {
      if (mounted) setState(() => _error = e.toString());
    } catch (e) {
      if (codeWasRedeemed && issuedToken != null && attemptStillCurrent()) {
        _issuedTokenForRetry = issuedToken;
        _issuedServerForRetry = attemptServer;
        _issuedCodeForRetry = attemptCode;
      }
      if (_claimedTokenForRetry != null) {
        try {
          await controller.abandonRecoveredPairing(_claimedTokenForRetry!);
          _claimedTokenForRetry = null;
        } catch (_) {
          // Keep the retry token scoped to this live screen if rollback cleanup fails.
        }
      }
      if (mounted) {
        setState(() {
          _error =
              'Pairing was issued, but local recovery failed. Existing data was preserved. Retry pairing to finish locally without redeeming the code again. Details: $e';
        });
      }
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
            if (_issuedTokenForRetry != null && _error != null)
              TextButton(
                onPressed: _busy ? null : _recoverKeepingData,
                child: const Text('Verify recovery and keep data'),
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
