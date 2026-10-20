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
class Api {
  Api(this.base, {this.token, http.Client? client})
    : _c = client ?? http.Client();
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
    final decoded = res.body.isEmpty
        ? <String, dynamic>{}
        : jsonDecode(res.body);
    if (res.statusCode >= 400) {
      throw ApiException(
        (decoded is Map ? decoded['error'] : null)?.toString() ??
            'Server error ${res.statusCode}',
      );
    }
    return decoded as Map<String, dynamic>;
  }

  Future<String> pair(String code, String deviceName) async {
    final r = await _send('POST', '/pair', {
      'code': code,
      'device_name': deviceName,
    });
    return r['token'] as String;
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
}
