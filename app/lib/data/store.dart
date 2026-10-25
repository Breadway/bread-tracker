import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:shared_preferences/shared_preferences.dart';

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

  void claimAuthGeneration() {
    _authGeneration++;
    _credentialEpoch++;
  }

  Future<bool> clearTokenIfCurrent(
    String expected,
    int expectedGeneration, {
    String? expectedServer,
  }) async {
    if (_authGeneration != expectedGeneration ||
        (expectedServer != null && serverUrl != expectedServer)) {
      return false;
    }
    final current = await _secure.read(key: 'token');
    if (_authGeneration != expectedGeneration ||
        current != expected ||
        (expectedServer != null && serverUrl != expectedServer)) {
      return false;
    }
    var cleared = false;
    await _serializeCredentialMutation(() async {
      if (_authGeneration != expectedGeneration) return;
      await _secure.delete(key: 'token');
      if (_authGeneration == expectedGeneration &&
          (expectedServer == null || serverUrl == expectedServer)) {
        cleared = true;
      }
    });
    return cleared;
  }

  Future<bool> clearTokenAndRunIfCurrent(
    String expected,
    int expectedEpoch,
    Future<void> Function() action,
  ) async {
    var ran = false;
    await _serializeCredentialMutation(() async {
      if (_credentialEpoch != expectedEpoch ||
          await _secure.read(key: 'token') != expected) {
        return;
      }
      await _secure.delete(key: 'token');
      await action();
      if (_credentialEpoch == expectedEpoch) ran = true;
    });
    return ran;
  }

  Future<bool> runIfTokenCurrent(
    String expected,
    int expectedGeneration,
    Future<void> Function() action,
  ) async {
    final current = await readTokenForGeneration(expectedGeneration);
    if (current != expected) return false;
    await action();
    return _authGeneration == expectedGeneration;
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
    Future<void> Function() action,
  ) async {
    var ran = false;
    await _serializeCredentialMutation(() async {
      if (_credentialEpoch != expectedEpoch ||
          await _secure.read(key: 'token') != null) {
        return;
      }
      await action();
      if (_credentialEpoch != expectedEpoch) return;
      ran = true;
    });
    return ran;
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
