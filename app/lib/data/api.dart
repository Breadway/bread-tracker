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

  Future<void> pushTest() => _send('POST', '/push/test');
}
