import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/push.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/server_settings.dart';

class PairedStore extends Store {
  PairedStore(super.prefs);
  @override
  Future<String?> get token async => 'tok';
}

/// A tiny stand-in for GET/PUT /settings that applies the same both-or-neither quiet rule.
class FakeSettingsServer {
  Map<String, dynamic> s = {
    'timezone': 'Australia/Perth',
    'quiet_start': '',
    'quiet_end': '',
    'reminders_enabled': true,
    'utc_offset_minutes': 480,
  };
  final puts = <Map<String, dynamic>>[];
  bool failGet = false;

  Future<http.Response> handle(http.Request r) async {
    if (failGet && r.method == 'GET') {
      return http.Response('<html>502</html>', 502);
    }
    if (r.method == 'PUT') {
      final p = jsonDecode(r.body) as Map<String, dynamic>;
      puts.add(p);
      final next = {...s, ...p};
      if ((next['quiet_start'] as String).isEmpty !=
          (next['quiet_end'] as String).isEmpty) {
        return http.Response(
          '{"error":"quiet_start and quiet_end must both be set or both be empty"}',
          400,
        );
      }
      if (p['timezone'] == 'Mars/Olympus') {
        return http.Response('{"error":"unknown timezone"}', 400);
      }
      s = next;
      if (p['timezone'] == 'Asia/Kolkata') s['utc_offset_minutes'] = 330;
    }
    return http.Response(jsonEncode(s), 200);
  }
}

Future<ProviderContainer> pump(
  WidgetTester tester,
  FakeSettingsServer server, {
  Duration phone = const Duration(hours: 8),
}) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final client = MockClient(server.handle);
  final container = ProviderContainer(
    overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(PairedStore(prefs)),
      pushProvider.overrideWithValue(NoPushService()),
      apiFactoryProvider.overrideWithValue(
        (b, t) => Api(b, token: t, client: client),
      ),
    ],
  );
  addTearDown(container.dispose);
  tester.view.physicalSize = const Size(1080, 2400);
  tester.view.devicePixelRatio = 2.625;
  addTearDown(tester.view.reset);
  await tester.pumpWidget(
    UncontrolledProviderScope(
      container: container,
      child: MaterialApp(
        home: Scaffold(
          body: SingleChildScrollView(
            child: ServerSettingsSection(phoneOffset: phone),
          ),
        ),
      ),
    ),
  );
  await tester.pumpAndSettle();
  return container;
}

void main() {
  testWidgets(
    'shows the server values and the reminder switch writes through',
    (tester) async {
      final server = FakeSettingsServer();
      await pump(tester, server);
      expect(find.text('Quiet hours are off'), findsOneWidget);
      expect(find.text('Server timezone: Australia/Perth'), findsOneWidget);
      expect(find.textContaining('UTC+08:00 now'), findsOneWidget);
      expect(
        find.textContaining('but the server uses'),
        findsNothing,
        reason: 'phone and server agree',
      );

      await tester.tap(find.byType(Switch));
      await tester.pumpAndSettle();
      expect(server.puts.single, {'reminders_enabled': false});
      expect(server.s['reminders_enabled'], false);
      expect(
        tester.widget<Switch>(find.byType(Switch)).value,
        isFalse,
        reason: 'UI reloads from the server',
      );
    },
  );

  testWidgets('quiet hours are set and cleared as a pair', (tester) async {
    final server = FakeSettingsServer();
    await pump(tester, server);
    await tester.tap(find.text('Quiet hours are off'));
    await tester.pumpAndSettle();
    // The time picker opens at the 22:00 default; confirm it.
    await tester.tap(find.text('OK'));
    await tester.pumpAndSettle();
    expect(server.puts.last, {'quiet_start': '22:00', 'quiet_end': '07:00'});
    expect(find.text('Quiet hours 22:00 to 07:00'), findsOneWidget);

    await tester.tap(find.text('Turn quiet hours off'));
    await tester.pumpAndSettle();
    expect(server.puts.last, {'quiet_start': '', 'quiet_end': ''});
    expect(find.text('Quiet hours are off'), findsOneWidget);
  });

  testWidgets('warns when the phone and server timezones differ', (
    tester,
  ) async {
    final server = FakeSettingsServer();
    await pump(tester, server, phone: const Duration(hours: -5));
    expect(
      find.textContaining(
        'This phone is on UTC-05:00 but the server uses UTC+08:00',
      ),
      findsOneWidget,
    );
    expect(find.textContaining('13.0 hours'), findsOneWidget);
  });

  testWidgets(
    'changing the timezone saves it and refreshes the offset; a bad zone shows the server error',
    (tester) async {
      final server = FakeSettingsServer();
      await pump(tester, server, phone: const Duration(hours: 5, minutes: 30));
      expect(find.textContaining('but the server uses'), findsOneWidget);

      Future<void> setZone(String zone) async {
        await tester.tap(find.text('Server timezone: ${server.s['timezone']}'));
        await tester.pumpAndSettle();
        await tester.enterText(find.byType(TextField), zone);
        await tester.tap(find.text('Save'));
        await tester.pumpAndSettle();
      }

      await setZone('Mars/Olympus');
      expect(find.text('unknown timezone'), findsOneWidget);
      expect(server.s['timezone'], 'Australia/Perth');

      await setZone('Asia/Kolkata');
      expect(find.text('Server timezone: Asia/Kolkata'), findsOneWidget);
      expect(
        find.textContaining('but the server uses'),
        findsNothing,
        reason: 'now +05:30 on both',
      );
    },
  );

  testWidgets('a failed load shows an error with Retry instead of crashing', (
    tester,
  ) async {
    final server = FakeSettingsServer()..failGet = true;
    final container = await pump(tester, server);
    expect(
      find.textContaining('Could not load reminder settings'),
      findsOneWidget,
    );
    server.failGet = false;
    await tester.tap(find.text('Retry'));
    await tester.pumpAndSettle();
    expect(find.text('Quiet hours are off'), findsOneWidget);
    expect(container.read(serverSettingsProvider).hasValue, isTrue);
  });
}
