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

class PairingPersistenceFailure implements Exception {
  const PairingPersistenceFailure(this.cause, {required this.restored});
  final Object cause;
  final bool restored;

  @override
  String toString() => '$cause';
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
  bool _preferencesReliable = true;
  static const defaultServer = 'https://tracker.breadway.dev';

  Future<String?> get token => _secure.read(key: 'token');
  Future<void> saveToken(String t) => _serializeCredentialMutation(() async {
    await _secure.write(key: 'token', value: t);
    if (await _secure.read(key: 'token') != t) {
      throw StateError('Secure credential was not durably saved.');
    }
  });

  Future<void> clearToken() => _serializeCredentialMutation(() async {
    await _secure.delete(key: 'token');
    if (await _secure.read(key: 'token') != null) {
      throw StateError('Secure credential was not durably removed.');
    }
  });

  int get credentialEpoch => _credentialEpoch;

  void claimCredentialOwnership() => claimAuthGeneration();

  int get authGeneration => _authGeneration;
  bool get preferencesReliable => _preferencesReliable;
  void markPreferencesUnreliable() => _preferencesReliable = false;

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
      if (!_preferencesReliable) {
        throw StateError('Shared preferences state is uncertain.');
      }
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
      var serverChanged = false;
      var tokenWriteAttempted = false;
      var cursorResetAttempted = false;
      try {
        serverChanged = previousServer != server;
        await setServerUrl(server);
        if (_authGeneration != owner.generation ||
            _credentialEpoch != owner.epoch ||
            serverUrl != server) {
          throw StateError(
            'Pairing ownership changed during local persistence.',
          );
        }
        if (previousToken != token) {
          tokenWriteAttempted = true;
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
        cursorResetAttempted = true;
        await resetSync();
        if (_authGeneration != owner.generation ||
            _credentialEpoch != owner.epoch ||
            serverUrl != server) {
          throw StateError(
            'Pairing ownership changed during local persistence.',
          );
        }
      } catch (cause) {
        var restored = false;
        var superseded = false;
        try {
          if (_authGeneration != owner.generation ||
              _credentialEpoch != owner.epoch) {
            superseded = true;
          } else {
            final currentToken = await _secure.read(key: 'token');
            var credentialRestored = false;
            if (tokenWriteAttempted && currentToken == token) {
              if (previousToken == null) {
                await _secure.delete(key: 'token');
              } else {
                await _secure.write(key: 'token', value: previousToken);
              }
              credentialRestored =
                  await _secure.read(key: 'token') == previousToken;
            } else {
              credentialRestored = currentToken == previousToken;
            }

            if (serverChanged) {
              await _restoreServerIfAttemptValue(previousServer, server);
            }
            final serverRestored = serverUrl == previousServer;
            if (cursorResetAttempted && credentialRestored) {
              await _restoreCursorIfAttemptValue(previousCursor, 0);
            }
            final cursorRestored = cursor == previousCursor;
            restored =
                credentialRestored &&
                serverRestored &&
                cursorRestored &&
                await _secure.read(key: 'token') == previousToken;
          }
        } catch (_) {
          restored = false;
        }
        throw PairingPersistenceFailure(
          cause,
          restored: restored || superseded,
        );
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
          !_preferencesReliable ||
          serverUrl != receipt.server ||
          await _secure.read(key: 'token') != receipt.token) {
        return;
      }
      var tokenRestored = false;
      if (receipt.previousToken == null) {
        await _secure.delete(key: 'token');
        tokenRestored = await _secure.read(key: 'token') == null;
      } else if (receipt.previousToken != receipt.token) {
        await _secure.write(key: 'token', value: receipt.previousToken);
        tokenRestored =
            await _secure.read(key: 'token') == receipt.previousToken;
      } else {
        tokenRestored =
            await _secure.read(key: 'token') == receipt.previousToken;
      }
      var preferencesRestored = false;
      if (tokenRestored &&
          _authGeneration == owner.generation &&
          _credentialEpoch == owner.epoch &&
          serverUrl == receipt.server &&
          await _secure.read(key: 'token') == receipt.previousToken) {
        await setServerUrl(receipt.previousServer);
        await setCursor(receipt.previousCursor);
        preferencesRestored =
            serverUrl == receipt.previousServer &&
            cursor == receipt.previousCursor;
      }
      rolledBack =
          _authGeneration == owner.generation &&
          _credentialEpoch == owner.epoch &&
          tokenRestored &&
          preferencesRestored &&
          serverUrl == receipt.previousServer &&
          await _secure.read(key: 'token') == receipt.previousToken &&
          cursor == receipt.previousCursor;
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
        if (await _secure.read(key: 'token') != null) {
          throw StateError('Issued pairing credential was not removed.');
        }
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
      cleared = await _secure.read(key: 'token') == null;
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
  Future<void> setServerUrl(String v) async {
    final normalized = v.trim().replaceAll(RegExp(r'/+$'), '');
    final previous = serverUrl;
    try {
      await _checkPreferenceWrite(
        () => prefs.setString('server', normalized),
        'Server preference was not durably saved.',
      );
      if (serverUrl != normalized) {
        throw StateError('Server preference was not durably saved.');
      }
    } catch (cause) {
      try {
        await _restoreServerIfAttemptValue(previous, normalized);
      } catch (_) {
        _preferencesReliable = false;
        throw StateError('Server preference failed and rollback is uncertain.');
      }
      Error.throwWithStackTrace(cause, StackTrace.current);
    }
  }

  int get cursor => prefs.getInt('cursor') ?? 0;
  Future<void> setCursor(int v) async {
    final previous = cursor;
    try {
      await _checkPreferenceWrite(
        () => prefs.setInt('cursor', v),
        'Sync cursor was not durably saved.',
      );
      if (cursor != v) {
        throw StateError('Sync cursor was not durably saved.');
      }
    } catch (cause) {
      try {
        await _restoreCursorIfAttemptValue(previous, v);
      } catch (_) {
        _preferencesReliable = false;
        throw StateError('Sync cursor write failed and rollback is uncertain.');
      }
      Error.throwWithStackTrace(cause, StackTrace.current);
    }
  }

  Future<void> resetSync() async {
    final previous = cursor;
    try {
      await _checkPreferenceWrite(
        () => prefs.remove('cursor'),
        'Sync cursor reset was not durably saved.',
      );
      if (cursor != 0) {
        throw StateError('Sync cursor reset was not durably saved.');
      }
    } catch (cause) {
      try {
        await _restoreCursorIfAttemptValue(previous, 0);
      } catch (_) {
        _preferencesReliable = false;
        throw StateError('Sync cursor reset failed and rollback is uncertain.');
      }
      Error.throwWithStackTrace(cause, StackTrace.current);
    }
  }

  Future<void> _restoreServerIfAttemptValue(
    String previous,
    String attempted,
  ) async {
    if (!_preferencesReliable) return;
    await prefs.reload();
    if (serverUrl != attempted || attempted == previous) return;
    if (!await prefs.setString('server', previous)) {
      await prefs.reload();
      if (serverUrl != previous) {
        throw StateError('Server rollback returned false and did not persist.');
      }
    } else if (serverUrl != previous) {
      await prefs.reload();
      if (serverUrl != previous) {
        throw StateError('Server rollback could not be verified.');
      }
    }
  }

  Future<void> _restoreCursorIfAttemptValue(int previous, int attempted) async {
    if (!_preferencesReliable) return;
    await prefs.reload();
    if (cursor != attempted || attempted == previous) return;
    if (!await prefs.setInt('cursor', previous)) {
      await prefs.reload();
      if (cursor != previous) {
        throw StateError('Cursor rollback returned false and did not persist.');
      }
    } else if (cursor != previous) {
      await prefs.reload();
      if (cursor != previous) {
        throw StateError('Cursor rollback could not be verified.');
      }
    }
  }

  Future<void> _checkPreferenceWrite(
    Future<bool> Function() write,
    String failure,
  ) async {
    if (!_preferencesReliable) {
      throw StateError('Shared preferences state is uncertain.');
    }
    try {
      if (await write()) return;
    } catch (_) {
      try {
        await _reloadPreferencesAfterFailure();
      } catch (_) {
        _preferencesReliable = false;
      }
      rethrow;
    }
    await _reloadPreferencesAfterFailure();
    throw StateError(failure);
  }

  Future<void> _reloadPreferencesAfterFailure() async {
    try {
      await prefs.reload();
    } catch (_) {
      _preferencesReliable = false;
      throw StateError(
        'Shared preferences write failed and persisted state could not be reloaded.',
      );
    }
  }

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
  Future<void> setSound(bool v) => _checkPreferenceWrite(
    () => prefs.setBool('sound', v),
    'Sound preference was not durably saved.',
  );
  bool get haptics => prefs.getBool('haptics') ?? true;
  Future<void> setHaptics(bool v) => _checkPreferenceWrite(
    () => prefs.setBool('haptics', v),
    'Haptics preference was not durably saved.',
  );
}
