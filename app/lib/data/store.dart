import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// Device credentials (secure storage) and small preferences.
class Store {
  Store(this.prefs, [FlutterSecureStorage? secure])
    : _secure = secure ?? const FlutterSecureStorage();
  final SharedPreferences prefs;
  final FlutterSecureStorage _secure;

  static const defaultServer = 'https://tracker.breadway.dev';

  Future<String?> get token => _secure.read(key: 'token');
  Future<void> saveToken(String t) => _secure.write(key: 'token', value: t);
  Future<void> clearToken() => _secure.delete(key: 'token');

  String get serverUrl => prefs.getString('server') ?? defaultServer;
  Future<void> setServerUrl(String v) =>
      prefs.setString('server', v.trim().replaceAll(RegExp(r'/+$'), ''));

  int get cursor => prefs.getInt('cursor') ?? 0;
  Future<void> setCursor(int v) => prefs.setInt('cursor', v);
  /// Identity of the server database the cursor belongs to (null until first sync).
  String? get epoch => prefs.getString('epoch');
  Future<void> setEpoch(String v) => prefs.setString('epoch', v);

  /// Forget the cursor and the server identity, so the next sync pulls everything.
  Future<void> resetSync() async {
    await prefs.remove('cursor');
    await prefs.remove('epoch');
  }

  bool get sound => prefs.getBool('sound') ?? true;
  Future<void> setSound(bool v) => prefs.setBool('sound', v);
  bool get haptics => prefs.getBool('haptics') ?? true;
  Future<void> setHaptics(bool v) => prefs.setBool('haptics', v);
}
