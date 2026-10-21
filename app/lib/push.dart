import 'dart:async';

import 'package:firebase_core/firebase_core.dart';
import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_local_notifications/flutter_local_notifications.dart';

import 'data/api.dart';
import 'data/store.dart';

/// Reminder delivery. The server sends standard FCM notification messages, which Android
/// shows itself when the app is closed; this class registers the device token with the
/// server and shows the same notification when the app is open in the foreground.
abstract class PushService {
  ValueListenable<PushStatus> get status;
  Future<void> init();

  /// Send the current FCM token to the server. Safe to call whenever; does nothing when
  /// push is unavailable or the device is not paired.
  Future<void> register({bool requestPermission = false});
}

enum PushStatus { unavailable, unpaired, denied, registering, ready, failed }

class NoPushService implements PushService {
  @override
  final ValueNotifier<PushStatus> status = ValueNotifier(
    PushStatus.unavailable,
  );
  @override
  Future<void> init() async {}
  @override
  Future<void> register({bool requestPermission = false}) async {}
}

/// Platform operations are separate so registration and retry logic can be tested.
abstract class PushDevice {
  Future<void> initialize();
  Future<bool> permitted({required bool request});
  Future<String?> token();
  Stream<String> get tokens;
  void dispose();
}

class FcmPushService implements PushService {
  FcmPushService(
    this.store, {
    PushDevice? device,
    Api Function(String, String)? apiFactory,
    this.onAuthLost,
  }) : _device = device ?? FirebasePushDevice(),
       _apiFactory = apiFactory ?? ((base, token) => Api(base, token: token));
  final Store store;
  final PushDevice _device;
  final Api Function(String, String) _apiFactory;
  final void Function()? onAuthLost;
  @override
  final ValueNotifier<PushStatus> status = ValueNotifier(
    PushStatus.unavailable,
  );
  bool _ready = false;
  Future<void>? _pending;
  StreamSubscription<String>? _refresh;

  @override
  Future<void> init() => register();

  @override
  Future<void> register({bool requestPermission = false}) {
    final pending = _pending;
    if (pending != null && requestPermission) {
      return pending.then((_) => register(requestPermission: true));
    }
    return _pending ??= _register(requestPermission)
        .whenComplete(() => _pending = null);
  }

  Future<void> _register(bool requestPermission) async {
    status.value = PushStatus.registering;
    try {
      if (!_ready) {
        await _device.initialize();
        _refresh = _device.tokens.listen(
          (_) => unawaited(register()),
          onError: (Object _) => status.value = PushStatus.failed,
        );
        _ready = true;
      }
      if (!await _device.permitted(request: requestPermission)) {
        status.value = PushStatus.denied;
        return;
      }
      final token = await store.token;
      if (token == null) {
        status.value = PushStatus.unpaired;
        return;
      }
      final fcm = await _device.token();
      if (fcm == null || fcm.isEmpty) {
        status.value = PushStatus.failed;
        return;
      }
      await _apiFactory(store.serverUrl, token).setFcmToken(fcm);
      status.value = PushStatus.ready;
    } on AuthLost {
      await store.clearToken();
      status.value = PushStatus.unpaired;
      _ready = false;
      onAuthLost?.call();
    } catch (_) {
      status.value = PushStatus.failed;
    }
  }

  void dispose() {
    _refresh?.cancel();
    _device.dispose();
  }
}

class FirebasePushDevice implements PushDevice {
  static const channelId = 'reminders';
  final _local = FlutterLocalNotificationsPlugin();
  StreamSubscription<RemoteMessage>? _foreground;

  @override
  Future<void> initialize() async {
    await Firebase.initializeApp();
    await _local.initialize(
      settings: const InitializationSettings(
        android: AndroidInitializationSettings('@drawable/ic_stat_bread'),
      ),
    );
    final android = _local
        .resolvePlatformSpecificImplementation<
          AndroidFlutterLocalNotificationsPlugin
        >();
    await android?.createNotificationChannel(
      const AndroidNotificationChannel(
        channelId,
        'Reminders',
        description: 'Chore, habit and to-do reminders',
        importance: Importance.high,
      ),
    );
    await _foreground?.cancel();
    _foreground = FirebaseMessaging.onMessage.listen((message) async {
      try {
        await _showForeground(message);
      } catch (error) {
        debugPrint('Could not display foreground reminder: $error');
      }
    });
  }

  @override
  Future<bool> permitted({required bool request}) async {
    final settings = request
        ? await FirebaseMessaging.instance.requestPermission()
        : await FirebaseMessaging.instance.getNotificationSettings();
    return settings.authorizationStatus == AuthorizationStatus.authorized ||
        settings.authorizationStatus == AuthorizationStatus.provisional;
  }

  // HTTP v1 on this server requires an FCM registration token, not an FID.
  @override
  // ignore: deprecated_member_use
  Future<String?> token() => FirebaseMessaging.instance.getToken();

  @override
  // ignore: deprecated_member_use
  Stream<String> get tokens => FirebaseMessaging.instance.onTokenRefresh;

  Future<void> _showForeground(RemoteMessage m) async {
    final n = m.notification;
    if (n == null) return;
    await _local.show(
      id: m.hashCode,
      title: n.title,
      body: n.body,
      notificationDetails: const NotificationDetails(
        android: AndroidNotificationDetails(
          channelId,
          'Reminders',
          importance: Importance.high,
          priority: Priority.high,
          icon: '@drawable/ic_stat_bread',
        ),
      ),
    );
  }

  @override
  void dispose() {
    _foreground?.cancel();
  }
}
