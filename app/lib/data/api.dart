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
