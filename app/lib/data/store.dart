import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// Device credentials (secure storage) and small preferences.
class Store {
  Store(this.prefs, [FlutterSecureStorage? secure])
    : _secure = secure ?? const FlutterSecureStorage();
  final SharedPreferences prefs;
  final FlutterSecureStorage _secure;
  Future<void> _credentialMutation = Future<void>.value();
  static const defaultServer = 'https://tracker.breadway.dev';

  Future<String?> get token => _secure.read(key: 'token');
  Future<void> saveToken(String t) =>
      _serializeCredentialMutation(() => _secure.write(key: 'token', value: t));

  Future<void> clearToken() =>
      _serializeCredentialMutation(() => _secure.delete(key: 'token'));

  /// Deletes only [expected], then keeps cleanup ahead of any re-pair write.
  Future<bool> clearTokenAndRunIfCurrent(
    String expected,
    Future<void> Function() action,
  ) async {
    var ran = false;
    await _serializeCredentialMutation(() async {
      if (await _secure.read(key: 'token') != expected) return;
      await _secure.delete(key: 'token');
      await action();
      ran = true;
    });
    return ran;
  }

  Future<bool> runIfTokenCurrent(
    String expected,
    Future<void> Function() action,
  ) async {
    var ran = false;
    await _serializeCredentialMutation(() async {
      if (await token != expected) return;
      await action();
      ran = true;
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

  bool get sound => prefs.getBool('sound') ?? true;
  Future<void> setSound(bool v) => prefs.setBool('sound', v);
  bool get haptics => prefs.getBool('haptics') ?? true;
  Future<void> setHaptics(bool v) => prefs.setBool('haptics', v);
}
