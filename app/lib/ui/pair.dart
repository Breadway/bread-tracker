import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../data/api.dart';
import '../data/store.dart';
import '../state.dart';
import '../theme.dart';
import 'widgets.dart';

/// Whether the device holds a token. Invalidate after pairing or unpairing.
final pairedProvider = FutureProvider<bool>((ref) async {
  ref.watch(pairedStateRevisionProvider);
  final controller = ref.watch(controllerProvider);
  final store = ref.watch(storeProvider);
  final generation = store.authGeneration;
  final appGeneration = controller.currentAuthGeneration;
  final token = await store.readTokenForGeneration(generation);
  final server = store.serverUrl;
  if (token != null &&
      !controller.hasEstablishedPairing &&
      !controller.isSignedOutForPairing) {
    await controller.restoreEstablishedPairing(generation, server);
  }
  final acceptedPairing = controller.hasEstablishedPairing
      ? controller.pairedCredentialAccepted &&
            controller.acceptedPairingGeneration == appGeneration &&
            controller.acceptedStoreGeneration == generation &&
            controller.acceptedPairingServer == server
      : controller.pairedCredentialAccepted &&
            !controller.isSignedOutForPairing;
  return token != null &&
      acceptedPairing &&
      store.preferencesReliable &&
      store.authGeneration == generation &&
      store.serverUrl == server &&
      controller.currentAuthGeneration == appGeneration &&
      (!controller.hasEstablishedPairing ||
          (controller.acceptedPairingGeneration == appGeneration &&
              controller.acceptedStoreGeneration == generation &&
              controller.acceptedPairingServer == store.serverUrl));
});

enum _Mode { join, pair }

class PairScreen extends ConsumerStatefulWidget {
  const PairScreen({super.key, this.onSkip, this.joinInitially = true});
  final VoidCallback? onSkip;
  final bool joinInitially;
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
  String? _issuedTokenForRetry;
  String? _issuedServerForRetry;
  String? _issuedCodeForRetry;
  AccountInfo? _issuedAccountForRetry;
  PairingOwnership? _retryOwnership;
  PairingPersistenceReceipt? _persistenceReceipt;
  int _pairAttemptGeneration = 0;
  bool _disposed = false;

  @override
  void initState() {
    super.initState();
    _mode = widget.joinInitially ? _Mode.join : _Mode.pair;
    _server.addListener(_onPairInputChanged);
    _code.addListener(_onPairInputChanged);
    _name.addListener(_onPairInputChanged);
  }

  void _onPairInputChanged() {
    if (_issuedTokenForRetry != null || _retryOwnership != null) {
      _invalidatePendingPairingResult();
    } else {
      _pairAttemptGeneration++;
    }
  }

  void _invalidatePendingPairingResult() {
    _pairAttemptGeneration++;
    _issuedTokenForRetry = null;
    _issuedAccountForRetry = null;
    _issuedServerForRetry = null;
    _issuedCodeForRetry = null;
    _retryOwnership = null;
    _persistenceReceipt = null;
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
      final pendingToken = _issuedTokenForRetry;
      final pendingServer = _issuedServerForRetry;
      if (pendingToken != null && pendingServer != null) {
        // The explicit local keep-data transition changed Store generation.
        // Rebind the live widget's cached exchange result to that new owner.
        // The token itself remains widget-scoped and is not written here.
        _retryOwnership = ref
            .read(storeProvider)
            .pairingOwnership(pendingServer);
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
    _name.removeListener(_onPairInputChanged);
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
    final recoveringIssuedWipe = ref
        .read(controllerProvider)
        .isWipeRecoveryPending;
    if ((!recoveringIssuedWipe &&
            _issuedTokenForRetry == null &&
            !await _confirmReplace()) ||
        !mounted) {
      return;
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
    final controllerGeneration = controller.currentAuthGeneration;
    final appServerBeforeAttempt = store.serverUrl;
    final matchingPendingResult =
        _issuedTokenForRetry != null &&
        _issuedServerForRetry == attemptServer &&
        _issuedCodeForRetry == attemptCode;
    final cachedOwnership = _retryOwnership;
    final retryingIssuedToken =
        matchingPendingResult &&
        cachedOwnership != null &&
        store.ownsPairing(cachedOwnership);
    var codeWasRedeemed = false;
    String? issuedToken;
    PairingOwnership? attemptOwnership;
    PairingOwnership? cleanupOwnership;
    var persistenceStarted = false;
    try {
      if (matchingPendingResult && !retryingIssuedToken) {
        _invalidatePendingPairingResult();
      }
      String? token = retryingIssuedToken ? _issuedTokenForRetry : null;
      if (!retryingIssuedToken) {
        final api = ref.read(apiFactoryProvider)(attemptServer, null);
        final AccountInfo account;
        if (join) {
          account = await api.signup(
            invite: attemptCode,
            name: name,
            deviceName: 'Android',
            timezone: await ref.read(phoneTimezoneProvider)(),
          );
        } else {
          account = await api.pair(attemptCode, 'Android');
        }
        if (!attemptStillCurrent()) return;
        codeWasRedeemed = true;
        issuedToken = account.token;
        _issuedAccountForRetry = account;
        token = account.token;
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
          final pendingOwnership = _retryOwnership;
          if (pendingOwnership == null ||
              !store.ownsPairing(pendingOwnership)) {
            throw StateError(
              'The cached pairing result no longer owns the current credential state. Verify recovery again.',
            );
          }
          attemptOwnership = pendingOwnership;
          cleanupOwnership = pendingOwnership;
          if (!await controller.claimPairingAfterRecovery()) {
            throw StateError(
              'Retained data recovery verification did not pass.',
            );
          }
          attemptOwnership = store.pairingOwnership(attemptServer);
          cleanupOwnership = attemptOwnership;
        } else {
          await controller.pairedAgain();
          attemptOwnership = store.pairingOwnership(attemptServer);
          cleanupOwnership = attemptOwnership;
        }
      } on Exception catch (e) {
        if (!attemptStillCurrent()) {
          final owner = cleanupOwnership;
          if (owner != null) {
            await store.abandonPairingAttempt(owner, pairedToken);
            if (store.authGeneration == owner.generation &&
                store.credentialEpoch == owner.epoch &&
                store.serverUrl == owner.server) {
              controller.pairingPersistenceFailedForCredentialOwner(
                controllerGeneration,
                owner.server,
              );
            }
          }
          _invalidatePendingPairingResult();
          return;
        }
        if (codeWasRedeemed && cleanupOwnership != null) {
          await store.abandonPairingAttempt(cleanupOwnership, pairedToken);
          if (store.authGeneration == cleanupOwnership.generation &&
              store.serverUrl == cleanupOwnership.server &&
              store.credentialEpoch == cleanupOwnership.epoch) {
            controller.pairingPersistenceFailedForCredentialOwner(
              controllerGeneration,
              cleanupOwnership.server,
            );
          }
        }
        _issuedTokenForRetry = pairedToken;
        _issuedServerForRetry = attemptServer;
        _issuedCodeForRetry = attemptCode;
        if (codeWasRedeemed && cleanupOwnership != null) {
          _retryOwnership = cleanupOwnership;
        }
        setState(
          () => _error =
              'Local recovery is incomplete. Existing data was preserved. Retry pairing to recover without redeeming the one-time code again. Details: $e',
        );
        return;
      }
      final owner = attemptOwnership;
      if (!attemptStillCurrent()) {
        await store.abandonPairingAttempt(owner, pairedToken);
        if (store.authGeneration == owner.generation &&
            store.credentialEpoch == owner.epoch &&
            store.serverUrl == owner.server) {
          controller.pairingPersistenceFailedForCredentialOwner(
            controllerGeneration,
            owner.server,
          );
        }
        return;
      }
      persistenceStarted = true;
      final account = _issuedAccountForRetry;
      if (account != null) {
        await controller.adoptAccount(
          server: attemptServer,
          token: pairedToken,
          account: account,
          ownership: owner,
          stillCurrent: attemptStillCurrent,
        );
      } else {
        _persistenceReceipt = await store.persistPairing(
          owner,
          attemptServer,
          pairedToken,
        );
      }
      if (!attemptStillCurrent()) {
        if (_persistenceReceipt != null) {
          await store.rollbackPairing(_persistenceReceipt!);
        }
        if (store.authGeneration == owner.generation &&
            store.credentialEpoch == owner.epoch &&
            store.serverUrl == owner.server) {
          controller.pairingPersistenceFailedForCredentialOwner(
            controllerGeneration,
            owner.server,
          );
        }
        _invalidatePendingPairingResult();
        return;
      }
      controller.pairingPersisted();
      _invalidatePendingPairingResult();
      // A (re)paired server may be empty or rebuilt, so offer it everything we have.
      // The cursor was reset above; marking rows dirty makes the next sync push them.
      await ref.read(dbProvider).markAllDirty();
      ref.read(pairedStateRevisionProvider.notifier).refresh();
      controller.syncNow();
      ref.read(pushProvider).register(requestPermission: true);
      if (mounted && Navigator.canPop(context)) Navigator.pop(context);
    } on ApiException catch (e) {
      if (mounted) {
        setState(() => _error = _friendly(e.message, join));
      }
    } on AuthLost catch (e) {
      if (mounted) setState(() => _error = e.toString());
    } catch (e) {
      final receipt = _persistenceReceipt;
      final owner = attemptOwnership;
      final storeRollbackVerified =
          e is PairingPersistenceFailure && e.restored;
      if (owner == null &&
          codeWasRedeemed &&
          issuedToken != null &&
          !persistenceStarted) {
        if (store.authGeneration == controllerGeneration &&
            store.serverUrl == appServerBeforeAttempt) {
          await store.abandonPairingAttempt(
            PairingOwnership(
              generation: store.authGeneration,
              epoch: store.credentialEpoch,
              server: store.serverUrl,
            ),
            issuedToken,
          );
        }
      }
      if (owner != null && issuedToken != null) {
        try {
          var cleanupVerified = false;
          if (receipt != null) {
            cleanupVerified = await store.rollbackPairing(receipt);
          } else if (persistenceStarted) {
            cleanupVerified = storeRollbackVerified;
          } else if (!persistenceStarted) {
            await store.abandonPairingAttempt(owner, issuedToken);
            cleanupVerified =
                store.authGeneration != owner.generation ||
                store.credentialEpoch != owner.epoch ||
                store.serverUrl != owner.server ||
                await store.token != issuedToken;
          }
          if (store.authGeneration == owner.generation &&
              store.credentialEpoch == owner.epoch &&
              store.serverUrl == owner.server) {
            controller.pairingPersistenceFailedForCredentialOwner(
              controllerGeneration,
              owner.server,
            );
            if (cleanupVerified && codeWasRedeemed && attemptStillCurrent()) {
              _retryOwnership = store.pairingOwnership(attemptServer);
              _issuedTokenForRetry = issuedToken;
              _issuedServerForRetry = attemptServer;
              _issuedCodeForRetry = attemptCode;
            }
          }
          if (!cleanupVerified) {
            controller.pairingPersistenceFailedForCredentialOwner(
              controllerGeneration,
              owner.server,
            );
            if (mounted) {
              setState(() {
                _error =
                    'Pairing was issued, but credential restoration could not be verified. Pairing remains closed and no cached retry is available. Details: $e';
                _issuedTokenForRetry = null;
                _issuedServerForRetry = null;
                _issuedCodeForRetry = null;
                _retryOwnership = null;
              });
            }
            return;
          }
          _persistenceReceipt = null;
        } catch (cleanupError) {
          controller.pairingPersistenceFailedForCredentialOwner(
            controllerGeneration,
            owner.server,
          );
          if (mounted) {
            setState(() {
              _error =
                  'Pairing was issued, but local cleanup could not be verified. Pairing remains unavailable in this screen. Verify server credentials before retrying. Details: $e; cleanup: $cleanupError';
              _issuedTokenForRetry = null;
              _issuedServerForRetry = null;
              _issuedCodeForRetry = null;
              _retryOwnership = null;
            });
          }
          return;
        }
      }
      if (mounted) {
        setState(() {
          _error =
              'Pairing was issued, but local recovery failed. Existing data was preserved. Retry pairing to finish locally without redeeming the code again. Details: $e';
        });
      }
    } finally {
      if (mounted && !_disposed) setState(() => _busy = false);
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
                ButtonSegment(
                  value: _Mode.join,
                  label: Text('Join with an invite'),
                ),
                ButtonSegment(
                  value: _Mode.pair,
                  label: Text('Pair with a code'),
                ),
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
            if (_issuedTokenForRetry != null && _error != null)
              TextButton(
                onPressed: _busy ? null : _recoverKeepingData,
                child: const Text('Verify recovery and keep data'),
              ),
            const SizedBox(height: 20),
            FilledButton(
              onPressed: _busy ? null : _submit,
              child: Text(
                _busy
                    ? (join ? 'Joining...' : 'Pairing...')
                    : (join ? 'Join' : 'Pair'),
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
