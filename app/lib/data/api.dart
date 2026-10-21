import 'dart:convert';

import 'package:http/http.dart' as http;

class AuthLost implements Exception {
  @override
  String toString() =>
      'This device is no longer paired. Pair it again in Settings.';
}

class ApiException implements Exception {
  ApiException(this.message);
  final String message;
  @override
  String toString() => message;
}

/// The account a device belongs to, as the server reports it.
class AccountInfo {
  const AccountInfo({required this.userId, required this.name, this.token});
  final int userId;
  final String name;

  /// Present only in replies that create a device (`/pair`, `/signup`).
  final String? token;

  factory AccountInfo.fromJson(Map<String, dynamic> j) {
    final id = (j['user_id'] as num?)?.toInt();
    final name = j['name'] as String?;
    if (id == null || name == null) {
      throw ApiException('This server is too old for this version of the app');
    }
    return AccountInfo(userId: id, name: name, token: j['token'] as String?);
  }
}

/// A short-lived code that adds another device to the signed-in account.
class PairCode {
  const PairCode(this.code, this.expiresInSecs);
  final String code;
  final int expiresInSecs;
}

/// The server-side reminder settings (`GET/PUT /settings`).
class ServerSettings {
  const ServerSettings({
    required this.timezone,
    required this.quietStart,
    required this.quietEnd,
    required this.remindersEnabled,
    required this.utcOffsetMinutes,
  });

  factory ServerSettings.fromJson(Map<String, dynamic> j) => ServerSettings(
    timezone: (j['timezone'] as String?) ?? '',
    quietStart: (j['quiet_start'] as String?) ?? '',
    quietEnd: (j['quiet_end'] as String?) ?? '',
    remindersEnabled: (j['reminders_enabled'] as bool?) ?? true,
    utcOffsetMinutes: (j['utc_offset_minutes'] as num?)?.toInt(),
  );

  final String timezone, quietStart, quietEnd;
  final bool remindersEnabled;

  /// The server timezone's current UTC offset; null when talking to an older server.
  final int? utcOffsetMinutes;

  bool get hasQuietHours => quietStart.isNotEmpty && quietEnd.isNotEmpty;

  /// True when the server's day and the phone's day can disagree right now.
  bool zoneDiffersFrom(Duration phoneOffset) =>
      utcOffsetMinutes != null && utcOffsetMinutes != phoneOffset.inMinutes;
}

/// Thin client for the Bread Tracker server.
///
/// An `Api` is cheap and short lived (the base URL and token can change), so it never owns
/// a connection pool. All instances share one process-wide [http.Client] unless a client is
/// passed in, which keeps a single set of keep-alive sockets instead of leaking an idle
/// `HttpClient` per sync.
class Api {
  Api(this.base, {this.token, http.Client? client})
    : _c = client ?? sharedClient;

  static http.Client? _shared;

  /// The client used when none is injected. Created on first use and never closed: it
  /// lives as long as the app process.
  static http.Client get sharedClient => _shared ??= http.Client();

  /// Replace (or with null, reset) the shared client. For tests.
  static set sharedClient(http.Client? c) => _shared = c;

  final String base;
  final String? token;
  final http.Client _c;

  Map<String, String> get _headers => {
    'content-type': 'application/json',
    if (token != null) 'authorization': 'Bearer $token',
  };

  Future<Map<String, dynamic>> _send(
    String method,
    String path, [
    Object? body,
  ]) async {
    final req = http.Request(method, Uri.parse('$base$path'))
      ..headers.addAll(_headers);
    if (body != null) req.body = jsonEncode(body);
    final http.Response res;
    try {
      res = await http.Response.fromStream(
        await _c.send(req).timeout(const Duration(seconds: 20)),
      );
    } catch (e) {
      throw ApiException('Cannot reach the server');
    }
    if (res.statusCode == 401 && token != null) throw AuthLost();
    // Decode defensively: an edge proxy (502 HTML) or a captive portal (200 HTML)
    // can answer with a body that is not the JSON object the server sends.
    Object? decoded;
    var parsed = false;
    if (res.body.isEmpty) {
      decoded = <String, dynamic>{};
      parsed = true;
    } else {
      try {
        decoded = jsonDecode(res.body);
        parsed = true;
      } on FormatException {
        parsed = false;
      }
    }
    if (res.statusCode >= 400) {
      throw ApiException(
        (parsed && decoded is Map ? decoded['error'] : null)?.toString() ??
            'Server error ${res.statusCode}',
      );
    }
    if (!parsed || decoded is! Map) {
      throw ApiException('Unexpected response from the server');
    }
    return decoded.cast<String, dynamic>();
  }

  /// Add this device to the account the code was made for.
  Future<AccountInfo> pair(String code, String deviceName) async => AccountInfo.fromJson(
    await _send('POST', '/pair', {'code': code, 'device_name': deviceName}),
  );

  /// Create an account from an invite code. [timezone] is the phone's IANA zone, if known.
  Future<AccountInfo> signup({
    required String invite,
    required String name,
    required String deviceName,
    String? timezone,
  }) async => AccountInfo.fromJson(
    await _send('POST', '/signup', {
      'invite': invite,
      'name': name,
      'device_name': deviceName,
      if (timezone != null && timezone.isNotEmpty) 'timezone': timezone,
    }),
  );

  /// Who this token belongs to.
  Future<AccountInfo> me() async => AccountInfo.fromJson(await _send('GET', '/me'));

  /// A code for adding another device to this account.
  Future<PairCode> newPairCode() async {
    final r = await _send('POST', '/pair-code');
    return PairCode(r['code'] as String, (r['expires_in_secs'] as num?)?.toInt() ?? 900);
  }

  Future<Map<String, dynamic>> sync(
    int since,
    List<Map<String, dynamic>> items,
    List<Map<String, dynamic>> completions,
  ) => _send('POST', '/sync', {
    'since': since,
    'items': items,
    'completions': completions,
  });

  Future<void> setFcmToken(String? token) =>
      _send('PUT', '/device', {'fcm_token': token});

  Future<ServerSettings> getSettings() async =>
      ServerSettings.fromJson(await _send('GET', '/settings'));

  /// Only the keys present in [patch] change. Keys: timezone, quiet_start, quiet_end,
  /// reminders_enabled.
  Future<ServerSettings> putSettings(Map<String, Object?> patch) async =>
      ServerSettings.fromJson(await _send('PUT', '/settings', patch));

  Future<void> pushTest() => _send('POST', '/push/test');

  /// Everything the server holds for this account, as the JSON document the server builds.
  Future<Map<String, dynamic>> exportData() => _send('GET', '/export');

  /// Delete this account and all of its data on the server. [confirm] must be the account name.
  Future<void> deleteAccount(String confirm) => _send('DELETE', '/account', {'confirm': confirm});

  /// Ask the server to revoke this device's token.
  Future<void> signOut() => _send('DELETE', '/device');
}
