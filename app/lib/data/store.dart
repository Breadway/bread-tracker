import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:shared_preferences/shared_preferences.dart';

class PairingOwnership {
  const PairingOwnership({
    required this.generation,
    required this.epoch,
    required this.server,
  });
  final int generation;
  final int epoch;
  final String server;
}

class PairingPersistenceReceipt {
  const PairingPersistenceReceipt({
    required this.ownership,
    required this.token,
    required this.server,
    required this.previousToken,
    required this.previousServer,
    required this.previousCursor,
  });
  final PairingOwnership ownership;
  final String token;
  final String server;
  final String? previousToken;
  final String previousServer;
  final int previousCursor;
}

/// Device credentials (secure storage) and small preferences.
class Store {
  Store(this.prefs, [FlutterSecureStorage? secure])
    : _secure = secure ?? const FlutterSecureStorage();
  final SharedPreferences prefs;
  final FlutterSecureStorage _secure;
  Future<void> _credentialMutation = Future<void>.value();
  int _credentialEpoch = 0;
  int _authGeneration = 0;
  static const defaultServer = 'https://tracker.breadway.dev';

  Future<String?> get token => _secure.read(key: 'token');
  Future<void> saveToken(String t) =>
      _serializeCredentialMutation(() => _secure.write(key: 'token', value: t));

  Future<void> clearToken() =>
      _serializeCredentialMutation(() => _secure.delete(key: 'token'));

  int get credentialEpoch => _credentialEpoch;

  void claimCredentialOwnership() => claimAuthGeneration();

  int get authGeneration => _authGeneration;

  PairingOwnership pairingOwnership(String server) => PairingOwnership(
    generation: _authGeneration,
    epoch: _credentialEpoch,
    server: serverUrl,
  );

  bool ownsPairing(PairingOwnership owner) =>
      _authGeneration == owner.generation && serverUrl == owner.server;

  Future<PairingPersistenceReceipt> persistPairing(
    PairingOwnership owner,
    String server,
    String token,
  ) async {
    late PairingPersistenceReceipt receipt;
    await _serializeCredentialMutation(() async {
      if (!ownsPairing(owner) || _credentialEpoch != owner.epoch) {
        throw StateError('Pairing ownership changed before local persistence.');
      }
      final previousToken = await _secure.read(key: 'token');
      final previousServer = serverUrl;
      final previousCursor = cursor;
      receipt = PairingPersistenceReceipt(
        ownership: owner,
        token: token,
        server: server,
        previousToken: previousToken,
        previousServer: previousServer,
        previousCursor: previousCursor,
      );
      try {
        await setServerUrl(server);
        if (_authGeneration != owner.generation ||
            _credentialEpoch != owner.epoch ||
            serverUrl != server) {
          throw StateError(
            'Pairing ownership changed during local persistence.',
          );
        }
        if (previousToken != token) {
          await _secure.write(key: 'token', value: token);
          if (_authGeneration != owner.generation ||
              _credentialEpoch != owner.epoch ||
              serverUrl != server ||
              await _secure.read(key: 'token') != token) {
            throw StateError(
              'Pairing ownership changed during local persistence.',
            );
          }
        }
        await resetSync();
        if (_authGeneration != owner.generation ||
            _credentialEpoch != owner.epoch ||
            serverUrl != server) {
          throw StateError(
            'Pairing ownership changed during local persistence.',
          );
        }
      } catch (_) {
        if (_authGeneration == owner.generation &&
            serverUrl == server &&
            _credentialEpoch == owner.epoch &&
            await _secure.read(key: 'token') == token) {
          var credentialRestored = false;
          if (previousToken == null) {
            await _secure.delete(key: 'token');
            credentialRestored = await _secure.read(key: 'token') == null;
          } else if (previousToken != token) {
            await _secure.write(key: 'token', value: previousToken);
            credentialRestored =
                await _secure.read(key: 'token') == previousToken;
          } else {
            credentialRestored =
                await _secure.read(key: 'token') == previousToken;
          }
          if (_authGeneration == owner.generation &&
              _credentialEpoch == owner.epoch &&
              serverUrl == server &&
              credentialRestored) {
            await setServerUrl(previousServer);
            await setCursor(previousCursor);
          }
        }
        rethrow;
      }
    });
    return receipt;
  }

  Future<bool> rollbackPairing(PairingPersistenceReceipt receipt) async {
    var rolledBack = false;
    await _serializeCredentialMutation(() async {
      final owner = receipt.ownership;
      if (_authGeneration != owner.generation ||
          _credentialEpoch != owner.epoch ||
          serverUrl != receipt.server ||
          await _secure.read(key: 'token') != receipt.token ||
          _authGeneration != owner.generation ||
          _credentialEpoch != owner.epoch ||
          serverUrl != receipt.server) {
        return;
      }
      if (receipt.previousToken == null) {
        await _secure.delete(key: 'token');
      } else if (receipt.previousToken != receipt.token) {
        await _secure.write(key: 'token', value: receipt.previousToken);
      }
      if (_authGeneration == owner.generation &&
          _credentialEpoch == owner.epoch &&
          serverUrl == receipt.server &&
          await _secure.read(key: 'token') == receipt.previousToken) {
        await setServerUrl(receipt.previousServer);
        await setCursor(receipt.previousCursor);
      }
      rolledBack =
          _authGeneration == owner.generation &&
          _credentialEpoch == owner.epoch &&
          serverUrl == receipt.previousServer &&
          await _secure.read(key: 'token') == receipt.previousToken;
    });
    return rolledBack;
  }

  Future<void> abandonPairingAttempt(PairingOwnership owner, String token) =>
      _serializeCredentialMutation(() async {
        if (_authGeneration != owner.generation ||
            _credentialEpoch != owner.epoch ||
            serverUrl != owner.server ||
            await _secure.read(key: 'token') != token) {
          return;
        }
        await _secure.delete(key: 'token');
        if (_authGeneration == owner.generation &&
            _credentialEpoch == owner.epoch &&
            serverUrl == owner.server &&
            await _secure.read(key: 'token') == null) {
          await setCursor(0);
        }
      });

  void claimAuthGeneration() {
    _authGeneration++;
    _credentialEpoch++;
  }

  void claimPairingOwnership() => claimAuthGeneration();

  Future<bool> clearTokenIfCurrent(
    String expected,
    int expectedGeneration, {
    String? expectedServer,
  }) async {
    var cleared = false;
    await _serializeCredentialMutation(() async {
      if (_authGeneration != expectedGeneration ||
          (expectedServer != null && serverUrl != expectedServer)) {
        return;
      }
      final server = serverUrl;
      final observed = await _secure.read(key: 'token');
      if (_authGeneration != expectedGeneration ||
          observed != expected ||
          serverUrl != server ||
          (expectedServer != null && server != expectedServer)) {
        return;
      }
      await _secure.delete(key: 'token');
      cleared = true;
    });
    return cleared;
  }

  Future<bool> clearTokenAndRunIfCurrent(
    String expected,
    int expectedEpoch,
    Future<void> Function() action, {
    int? expectedGeneration,
    String? expectedServer,
  }) async {
    final requestGeneration = expectedGeneration ?? _authGeneration;
    final requestServer = expectedServer ?? serverUrl;
    var ran = false;
    await _serializeCredentialMutation(() async {
      final generation = requestGeneration;
      final server = requestServer;
      if (_credentialEpoch != expectedEpoch ||
          _authGeneration != generation ||
          serverUrl != server) {
        return;
      }
      final current = await _secure.read(key: 'token');
      if (current != expected ||
          _credentialEpoch != expectedEpoch ||
          _authGeneration != generation ||
          serverUrl != server) {
        return;
      }
      await _secure.delete(key: 'token');
      if (_credentialEpoch != expectedEpoch ||
          _authGeneration != generation ||
          serverUrl != server) {
        return;
      }
      await action();
      ran =
          _credentialEpoch == expectedEpoch &&
          _authGeneration == generation &&
          serverUrl == server;
    });
    return ran;
  }

  Future<bool> runIfTokenCurrent(
    String expected,
    int expectedGeneration,
    Future<void> Function() action,
  ) async {
    var ran = false;
    await _serializeCredentialMutation(() async {
      final server = serverUrl;
      final current = await _secure.read(key: 'token');
      if (_authGeneration != expectedGeneration ||
          current != expected ||
          serverUrl != server) {
        return;
      }
      await action();
      ran = _authGeneration == expectedGeneration && serverUrl == server;
    });
    return ran;
  }

  Future<bool> runIfTokenless(Future<void> Function() action) async {
    var ran = false;
    await _serializeCredentialMutation(() async {
      if (await _secure.read(key: 'token') != null) return;
      await action();
      ran = true;
    });
    return ran;
  }

  Future<bool> runIfTokenlessAndEpochCurrent(
    int expectedEpoch,
    Future<void> Function() action, {
    int? expectedGeneration,
    String? expectedServer,
  }) async {
    final requestGeneration = expectedGeneration ?? _authGeneration;
    final requestServer = expectedServer ?? serverUrl;
    var ran = false;
    await _serializeCredentialMutation(() async {
      final generation = requestGeneration;
      final server = requestServer;
      if (_credentialEpoch != expectedEpoch ||
          _authGeneration != generation ||
          serverUrl != server) {
        return;
      }
      final current = await _secure.read(key: 'token');
      if (current != null ||
          _credentialEpoch != expectedEpoch ||
          _authGeneration != generation ||
          serverUrl != server) {
        return;
      }
      await action();
      ran =
          _credentialEpoch == expectedEpoch &&
          _authGeneration == generation &&
          serverUrl == server;
    });
    return ran;
  }

  Future<bool> runIfCredentialCurrentForRecovery(
    String? expectedToken,
    int expectedGeneration,
    String expectedServer,
    Future<bool> Function() verifyAndRecover,
  ) async {
    var recovered = false;
    await _serializeCredentialMutation(() async {
      if (_authGeneration != expectedGeneration ||
          serverUrl != expectedServer ||
          await _secure.read(key: 'token') != expectedToken ||
          _authGeneration != expectedGeneration ||
          serverUrl != expectedServer) {
        return;
      }
      recovered = await verifyAndRecover();
      if (_authGeneration != expectedGeneration ||
          serverUrl != expectedServer) {
        recovered = false;
      }
    });
    return recovered;
  }

  Future<String?> readTokenForGeneration(int expectedGeneration) async {
    final value = await token;
    if (_authGeneration != expectedGeneration) return null;
    return value;
  }

  Future<bool> clearTokenIfCurrentForGeneration(
    String expected,
    int expectedGeneration, {
    String? expectedServer,
  }) => clearTokenIfCurrent(
    expected,
    expectedGeneration,
    expectedServer: expectedServer,
  );

  Future<void> _serializeCredentialMutation(Future<void> Function() action) {
    final next = _credentialMutation.then((_) => action());
    _credentialMutation = next.catchError((Object _) {});
    return next;
  }

  String get serverUrl => prefs.getString('server') ?? defaultServer;
  Future<void> setServerUrl(String v) =>
      prefs.setString('server', v.trim().replaceAll(RegExp(r'/+$'), ''));

  int get cursor => prefs.getInt('cursor') ?? 0;
  Future<void> setCursor(int v) => prefs.setInt('cursor', v);
  Future<void> resetSync() => prefs.remove('cursor');

  Future<bool> setCursorIfGeneration(
    int value,
    int expectedGeneration, {
    String? expectedServer,
  }) async {
    var written = false;
    await _serializeCredentialMutation(() async {
      if (_authGeneration != expectedGeneration ||
          (expectedServer != null && serverUrl != expectedServer)) {
        return;
      }
      await setCursor(value);
      written =
          _authGeneration == expectedGeneration &&
          (expectedServer == null || serverUrl == expectedServer);
    });
    return written;
  }

  Future<bool> resetSyncIfGeneration(
    int expectedGeneration, {
    String? expectedServer,
  }) async {
    var reset = false;
    await _serializeCredentialMutation(() async {
      if (_authGeneration != expectedGeneration ||
          (expectedServer != null && serverUrl != expectedServer)) {
        return;
      }
      await resetSync();
      reset =
          _authGeneration == expectedGeneration &&
          (expectedServer == null || serverUrl == expectedServer);
    });
    return reset;
  }

  bool get sound => prefs.getBool('sound') ?? true;
  Future<void> setSound(bool v) => prefs.setBool('sound', v);
  bool get haptics => prefs.getBool('haptics') ?? true;
  Future<void> setHaptics(bool v) => prefs.setBool('haptics', v);
}
