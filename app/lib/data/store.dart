import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// Which account on which server the data on this phone belongs to.
class Account {
  const Account(this.server, this.userId, this.name);
  final String server;
  final int userId;
  final String name;

  /// Same account: same server (ignoring a trailing slash) and same user id.
  bool sameAs(String otherServer, int otherUserId) =>
      userId == otherUserId && _norm(server) == _norm(otherServer);

  static String _norm(String s) => s.trim().replaceAll(RegExp(r'/+$'), '').toLowerCase();
}

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

  /// The account the local items and ticks belong to: the server they came from and the user
  /// id on it. Set when the phone joins or pairs, kept through sign out (the data stays), and
  /// cleared only when the local data is deleted. Not a secret.
  Account? get account {
    final server = prefs.getString('account_server');
    final id = prefs.getInt('account_user');
    if (server == null || id == null) return null;
    return Account(server, id, prefs.getString('account_name') ?? '');
  }

  Future<void> setAccount(Account a) async {
    await prefs.setString('account_server', a.server);
    await prefs.setInt('account_user', a.userId);
    await prefs.setString('account_name', a.name);
  }

  Future<void> clearAccount() async {
    await prefs.remove('account_server');
    await prefs.remove('account_user');
    await prefs.remove('account_name');
  }

  bool get sound => prefs.getBool('sound') ?? true;
  Future<void> setSound(bool v) => prefs.setBool('sound', v);
  bool get haptics => prefs.getBool('haptics') ?? true;
  Future<void> setHaptics(bool v) => prefs.setBool('haptics', v);
}
