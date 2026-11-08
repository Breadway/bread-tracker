import 'dart:async';

import 'package:firebase_core/firebase_core.dart';
import 'package:firebase_messaging/firebase_messaging.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter_local_notifications/flutter_local_notifications.dart';

import 'data/api.dart';
import 'data/store.dart';
import 'reminders.dart';

/// Reminder delivery. The server sends standard FCM notification messages, which Android
/// shows itself when the app is closed; this class registers the device token with the
/// server and shows the same notification when the app is open in the foreground.
abstract class PushService {
  ValueListenable<PushStatus> get status;

  /// Reminders the user opened by tapping the notification body. Includes the one that
  /// launched the app, delivered once after [init].
  Stream<ReminderPayload> get taps;

  /// Drop an in-flight tap marker so a later consumer can receive it again.
  void releaseTapDelivery();
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
  Stream<ReminderPayload> get taps => const Stream.empty();
  @override
  void releaseTapDelivery() {}
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

  /// Reminders opened from a notification, while running or from a cold start.
  Stream<ReminderPayload> get taps;
  void dispose();
}

class FcmPushService implements PushService {
  FcmPushService(
    this.store, {
    PushDevice? device,
    Api Function(String, String)? apiFactory,
  }) : device = device ?? FirebasePushDevice(),
       _apiFactory = apiFactory ?? ((base, token) => Api(base, token: token));
  final Store store;
  final PushDevice device;
  final Api Function(String, String) _apiFactory;
  @override
  final ValueNotifier<PushStatus> status = ValueNotifier(
    PushStatus.unavailable,
  );
  bool _ready = false;
  Future<void>? _pending;
  StreamSubscription<String>? _refresh;

  @override
  Stream<ReminderPayload> get taps => device.taps;

  void acknowledgeTap(ReminderPayload payload) {
    final d = device;
    if (d is FirebasePushDevice) d.acknowledgeTap(payload);
  }

  void replayPendingTap() {
    final d = device;
    if (d is FirebasePushDevice) d.replayPendingTap();
  }

  @override
  void releaseTapDelivery() {
    final d = device;
    if (d is FirebasePushDevice) d.releaseTapDelivery();
  }

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
        await device.initialize();
        _refresh = device.tokens.listen(
          (_) => unawaited(register()),
          onError: (Object _) => status.value = PushStatus.failed,
        );
        _ready = true;
      }
      if (!await device.permitted(request: requestPermission)) {
        status.value = PushStatus.denied;
        return;
      }
      final token = await store.token;
      if (token == null) {
        status.value = PushStatus.unpaired;
        return;
      }
      final fcm = await device.token();
      if (fcm == null || fcm.isEmpty) {
        status.value = PushStatus.failed;
        return;
      }
      await _apiFactory(store.serverUrl, token).setFcmToken(fcm);
      status.value = PushStatus.ready;
    } on AuthLost {
      await store.clearToken();
      status.value = PushStatus.unpaired;
    } catch (_) {
      status.value = PushStatus.failed;
    }
  }

  void dispose() {
    _refresh?.cancel();
    device.dispose();
  }
}

class FirebasePushDevice implements PushDevice {
  static const channelId = reminderChannelId;
  final _local = FlutterLocalNotificationsPlugin();
  final StreamController<ReminderPayload> _taps =
      StreamController<ReminderPayload>.broadcast();
  ReminderPayload? _pendingLaunch;
  final Set<ReminderPayload> _inFlightTaps = {};
  StreamSubscription<RemoteMessage>? _foreground, _opened;

  /// Tests exercise this exact production buffering/replay code with a delayed
  /// launch-data provider, matching Firebase initialization completing after Shell.
  FirebasePushDevice({
    @visibleForTesting Future<ReminderPayload?> Function()? initialTap,
  }) : _initialTap = initialTap ?? _firebaseInitialTap;
  final Future<ReminderPayload?> Function() _initialTap;

  static Future<ReminderPayload?> _firebaseInitialTap() async {
    final message = await FirebaseMessaging.instance.getInitialMessage();
    if (message != null) return ReminderPayload.fromData(message.data);
    final local = await FlutterLocalNotificationsPlugin()
        .getNotificationAppLaunchDetails();
    return (local?.didNotificationLaunchApp ?? false) &&
            (local?.notificationResponse?.actionId ?? '').isEmpty
        ? ReminderPayload.decode(local?.notificationResponse?.payload)
        : null;
  }

  @override
  Stream<ReminderPayload> get taps => _taps.stream;

  void acknowledgeTap(ReminderPayload payload) {
    _inFlightTaps.remove(payload);
    if (_pendingLaunch == payload) _pendingLaunch = null;
  }

  void releaseTapDelivery() {
    _inFlightTaps.clear();
  }

  @override
  Future<void> initialize() async {
    await Firebase.initializeApp();
    await initLocalNotifications(
      _local,
      // Tap on the body of a notification we built (foreground or data mode).
      onResponse: (r) {
        if (r.actionId != null && r.actionId!.isNotEmpty) return;
        final p = ReminderPayload.decode(r.payload);
        if (p != null) _rememberTap(p);
      },
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

    // Tap on a notification Android showed itself (notification mode), app in background.
    await _opened?.cancel();
    _opened = FirebaseMessaging.onMessageOpenedApp.listen((m) {
      final p = ReminderPayload.fromData(m.data);
      if (p != null) _rememberTap(p);
    });

    // App launched by a notification tap while it was not running.
    await _captureInitialTap();
  }

  @visibleForTesting
  Future<void> captureInitialTapForTest(
    Future<ReminderPayload?> Function() initialTap,
  ) => _captureInitialTap(initialTap);

  Future<void> _captureInitialTap([
    Future<ReminderPayload?> Function()? initialTap,
  ]) async {
    final launch = await (initialTap ?? _initialTap)();
    if (launch != null) _pendingLaunch = launch;
    if (_pendingLaunch case final pending?) _rememberTap(pending);
  }

  void _rememberTap(ReminderPayload payload) {
    _pendingLaunch = payload;
    if (_taps.hasListener) {
      _deliverPending(payload);
    }
  }

  void _deliverPending(ReminderPayload payload) {
    if (_inFlightTaps.add(payload)) _taps.add(payload);
  }

  void replayPendingTap() {
    final pending = _pendingLaunch;
    if (pending != null && _taps.hasListener) {
      _deliverPending(pending);
    }
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
    final p = ReminderPayload.fromData(m.data);
    final n = m.notification;
    if (p != null) {
      await showReminderNotification(
        _local,
        ReminderPayload(
          itemId: p.itemId,
          date: p.date,
          kind: p.kind,
          title: n?.title ?? p.title,
          body: n?.body ?? p.body,
        ),
      );
      return;
    }
    if (n == null) return;
    await _local.show(
      id: m.hashCode,
      title: n.title,
      body: n.body,
      notificationDetails: reminderNotificationDetails(),
    );
  }

  @override
  void dispose() {
    _foreground?.cancel();
    _opened?.cancel();
    _pendingLaunch = null;
    _taps.close();
  }
}
