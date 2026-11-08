import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/push.dart';
import 'package:tracker/reminders.dart';

class MemoryStore extends Store {
  MemoryStore(super.prefs);
  String? credential = 'paired';
  @override
  Future<String?> get token async => credential;
  @override
  Future<void> clearToken() async => credential = null;
  @override
  Future<bool> clearTokenIfCurrentForGeneration(
    String expected,
    int expectedGeneration, {
    String? expectedServer,
  }) async {
    if (credential != expected) return false;
    credential = null;
    return true;
  }

  @override
  Future<void> saveToken(String token) async => credential = token;
}

class FakeDevice implements PushDevice {
  final events = StreamController<String>.broadcast();
  bool allowed = true;
  bool broken = false;
  int prompts = 0;
  String? registration = 'fcm-token';
  Completer<void>? tokenStarted;
  Completer<void>? releaseToken;
  @override
  Future<void> initialize() async {
    if (broken) throw StateError('Firebase unavailable');
  }

  @override
  Future<bool> permitted({required bool request}) async {
    if (request) prompts++;
    return allowed;
  }

  @override
  Future<String?> token() async {
    tokenStarted?.complete();
    if (releaseToken != null) await releaseToken!.future;
    return registration;
  }

  @override
  Stream<String> get tokens => events.stream;
  @override
  Stream<ReminderPayload> get taps => const Stream.empty();
  @override
  void dispose() => unawaited(events.close());
}

class ColdStartDevice extends FakeDevice {
  final controller = StreamController<ReminderPayload>.broadcast();
  ReminderPayload? pending;
  @override
  Stream<ReminderPayload> get taps => controller.stream;
  void deliver(ReminderPayload payload) {
    pending = payload;
    if (controller.hasListener) controller.add(payload);
  }
}

class RegistrationApi extends Api {
  RegistrationApi() : super('https://example.invalid');
  bool offline = false;
  bool revoked = false;
  Completer<void>? callStarted;
  Completer<void>? releaseResponse;
  Completer<void>? requestStarted;
  Completer<void>? releaseAuthLost;
  final sent = <String?>[];
  @override
  Future<void> setFcmToken(String? token) async {
    callStarted?.complete();
    if (releaseResponse != null) {
      await releaseResponse!.future;
      throw AuthLost();
    }
    if (offline) throw ApiException('offline');
    if (releaseAuthLost != null) {
      requestStarted!.complete();
      await releaseAuthLost!.future;
      throw AuthLost();
    }
    if (revoked) throw AuthLost();
    sent.add(token);
  }
}

void main() {
  late MemoryStore store;
  late FakeDevice device;
  late RegistrationApi api;
  late FcmPushService push;

  setUp(() async {
    TestWidgetsFlutterBinding.ensureInitialized();
    SharedPreferences.setMockInitialValues({});
    store = MemoryStore(await SharedPreferences.getInstance());
    device = FakeDevice();
    api = RegistrationApi();
    push = FcmPushService(store, device: device, apiFactory: (_, _) => api);
  });
  tearDown(() => push.dispose());

  test('registers paired device without prompting at startup', () async {
    await push.init();
    expect(push.status.value, PushStatus.ready);
    expect(api.sent, ['fcm-token']);
    expect(device.prompts, 0);
  });

  test(
    'old bearer is never sent to a replacement server after delayed FCM lookup',
    () async {
      device.tokenStarted = Completer<void>();
      device.releaseToken = Completer<void>();
      final destinations = <String>[];
      final guardedPush = FcmPushService(
        store,
        device: device,
        apiFactory: (server, token) {
          destinations.add('$server|$token');
          return api;
        },
      );
      final pending = guardedPush.init();
      await device.tokenStarted!.future;
      store.claimPairingOwnership();
      await store.setServerUrl('https://replacement.invalid');
      store.credential = 'replacement';
      device.releaseToken!.complete();
      await pending;
      expect(destinations, isEmpty);
      expect(api.sent, isEmpty);
      guardedPush.dispose();
    },
  );

  test('permission denial is visible and can be retried explicitly', () async {
    device.allowed = false;
    await push.init();
    expect(push.status.value, PushStatus.denied);
    expect(api.sent, isEmpty);
    device.allowed = true;
    await push.register(requestPermission: true);
    expect(device.prompts, 1);
    expect(push.status.value, PushStatus.ready);
  });

  test('unpaired and revoked devices are not reported as ready', () async {
    store.credential = null;
    await push.init();
    expect(push.status.value, PushStatus.unpaired);
    expect(api.sent, isEmpty);
    store.credential = 'new-token';
    api.revoked = true;
    await push.register();
    expect(push.status.value, PushStatus.unpaired);
  });

  test('delayed old registration 401 cannot clear a new pairing', () async {
    api
      ..callStarted = Completer<void>()
      ..releaseResponse = Completer<void>();
    final pending = push.init();
    await api.callStarted!.future;
    store.credential = 'new-token';
    api.releaseResponse!.complete();
    await pending;
    expect(store.credential, 'new-token');
    expect(push.status.value, PushStatus.registering);
  });

  test('network and initialization failures can be retried', () async {
    device.broken = true;
    await push.init();
    expect(push.status.value, PushStatus.failed);
    device.broken = false;
    api.offline = true;
    await push.register();
    expect(push.status.value, PushStatus.failed);
    api.offline = false;
    await push.register();
    expect(push.status.value, PushStatus.ready);
  });

  test(
    'missing token fails and concurrent registration is coalesced',
    () async {
      device.registration = null;
      await push.init();
      expect(push.status.value, PushStatus.failed);
      device.registration = 'fresh-token';
      await Future.wait([push.register(), push.register()]);
      expect(api.sent, ['fresh-token']);
    },
  );

  test('token refresh registers the updated target', () async {
    await push.init();
    device.registration = 'rotated';
    device.events.add('rotated');
    await Future<void>.delayed(Duration.zero);
    expect(api.sent, ['fcm-token', 'rotated']);
  });

  test('explicit permission request is not lost during startup', () async {
    final startup = push.init();
    final enable = push.register(requestPermission: true);
    await Future.wait([startup, enable]);
    expect(device.prompts, 1);
    expect(push.status.value, PushStatus.ready);
  });

  test(
    'production launch tap replays behind delayed Gate and remount',
    () async {
      final pending = const ReminderPayload(
        itemId: 'item',
        date: '2026-10-21',
        kind: 'chore',
      );
      final gate = Completer<void>();
      final device = FirebasePushDevice();
      final startup = device.captureInitialTapForTest(() async {
        await gate.future;
        return pending;
      });
      await Future<void>.delayed(Duration.zero);
      final firstSeen = <ReminderPayload>[];
      final first = device.taps.listen(firstSeen.add);
      gate.complete();
      await startup;
      device.replayPendingTap();
      await Future<void>.delayed(Duration.zero);
      expect(firstSeen, [pending]);
      await first.cancel();
      device.releaseTapDelivery();
      final secondSeen = <ReminderPayload>[];
      final second = device.taps.listen((p) {
        secondSeen.add(p);
        device.acknowledgeTap(p);
      });
      device.replayPendingTap();
      await Future<void>.delayed(Duration.zero);
      expect(secondSeen, [pending]);
      await second.cancel();
      device.dispose();
    },
  );

  test(
    'AuthLost during push registration clears the dead pairing credential',
    () async {
      api.revoked = true;
      await push.init();
      expect(push.status.value, PushStatus.unpaired);
      expect(await store.token, isNull);
    },
  );
}
