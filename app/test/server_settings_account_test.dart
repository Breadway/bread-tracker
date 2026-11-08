import 'dart:async';
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

const _server = 'https://tracker.example';

class SwitchingStore extends Store {
  SwitchingStore(super.prefs);

  String currentToken = 'alice';

  @override
  Future<String?> get token async => currentToken;
}

/// Serves account-specific settings and records which token every request used.
class AccountSettingsServer {
  final byToken = <String, Map<String, dynamic>>{
    'alice': {
      'timezone': 'Australia/Perth',
      'quiet_start': '',
      'quiet_end': '',
      'reminders_enabled': true,
      'utc_offset_minutes': 480,
    },
    'bob': {
      'timezone': 'UTC',
      'quiet_start': '',
      'quiet_end': '',
      'reminders_enabled': true,
      'utc_offset_minutes': 0,
    },
  };

  final gets = <String>[];
  final puts = <(String, Map<String, dynamic>)>[];
  Completer<void>? holdBobGet;

  String _token(http.Request r) =>
      (r.headers['authorization'] ?? '').replaceFirst('Bearer ', '');

  Future<http.Response> handle(http.Request r) async {
    final token = _token(r);
    if (r.method == 'GET') {
      gets.add(token);
      if (token == 'bob') await holdBobGet?.future;
      return http.Response(jsonEncode(byToken[token]), 200);
    }
    if (r.method == 'PUT') {
      final patch = jsonDecode(r.body) as Map<String, dynamic>;
      puts.add((token, patch));
      byToken[token] = {...byToken[token]!, ...patch};
      return http.Response(jsonEncode(byToken[token]), 200);
    }
    return http.Response('{}', 404);
  }
}

Future<(ProviderContainer, SwitchingStore)> pumpSettings(
  WidgetTester tester,
  AccountSettingsServer server,
) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final store = SwitchingStore(prefs);
  await store.setAccount(const Account(_server, 1, 'Alice'));
  final container = ProviderContainer(
    overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(store),
      pushProvider.overrideWithValue(NoPushService()),
      apiFactoryProvider.overrideWithValue(
        (b, t) => Api(b, token: t, client: MockClient(server.handle)),
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
      child: const MaterialApp(
        home: Scaffold(
          body: SingleChildScrollView(
            child: ServerSettingsSection(phoneOffset: Duration(hours: 8)),
          ),
        ),
      ),
    ),
  );
  await tester.pumpAndSettle();
  return (container, store);
}

void main() {
  testWidgets(
    'the mounted section reloads settings for the new account after adopt',
    (tester) async {
      final server = AccountSettingsServer();
      final (container, store) = await pumpSettings(tester, server);
      expect(find.text('Server timezone: Australia/Perth'), findsOneWidget);

      store.currentToken = 'bob';
      await store.setAccount(const Account(_server, 2, 'Bob'));
      store.claimPairingOwnership();
      container.read(pairedStateRevisionProvider.notifier).refresh();
      await tester.pumpAndSettle();

      expect(find.text('Server timezone: UTC'), findsOneWidget);
      expect(find.text('Server timezone: Australia/Perth'), findsNothing);
      expect(server.gets, ['alice', 'bob']);
    },
  );

  testWidgets(
    'the previous account settings are not interactive while the new account loads',
    (tester) async {
      final server = AccountSettingsServer();
      final hold = Completer<void>();
      server.holdBobGet = hold;
      final (container, store) = await pumpSettings(tester, server);

      store.currentToken = 'bob';
      await store.setAccount(const Account(_server, 2, 'Bob'));
      store.claimPairingOwnership();
      container.read(pairedStateRevisionProvider.notifier).refresh();
      await tester.pump();
      await tester.pump();

      expect(find.text('Server timezone: Australia/Perth'), findsNothing);
      expect(find.text('Loading reminder settings...'), findsOneWidget);

      hold.complete();
      await tester.pumpAndSettle();
      expect(find.text('Server timezone: UTC'), findsOneWidget);
    },
  );

  testWidgets(
    'a save started for the previous account is rejected before the revision refresh',
    (tester) async {
      final server = AccountSettingsServer();
      final (_, store) = await pumpSettings(tester, server);

      await tester.tap(find.text('Server timezone: Australia/Perth'));
      await tester.pumpAndSettle();
      await tester.enterText(find.byType(TextField), 'Europe/Berlin');

      // Mid-adopt: the credential generation moved to Bob, but the UI revision
      // has not refreshed yet, so the old editor is still mounted and callable.
      store.currentToken = 'bob';
      await store.setAccount(const Account(_server, 2, 'Bob'));
      store.claimPairingOwnership();

      await tester.tap(find.text('Save'));
      await tester.pumpAndSettle();

      expect(server.puts, isEmpty, reason: 'no write through the new token');
      expect(server.byToken['bob']!['timezone'], 'UTC');
      expect(
        find.text('Account changed. Settings were not saved.'),
        findsOneWidget,
      );
    },
  );

  testWidgets(
    'an editor left open across an account switch cannot write to the new account',
    (tester) async {
      final server = AccountSettingsServer();
      final (container, store) = await pumpSettings(tester, server);

      await tester.tap(find.text('Server timezone: Australia/Perth'));
      await tester.pumpAndSettle();
      await tester.enterText(find.byType(TextField), 'Europe/Berlin');

      store.currentToken = 'bob';
      await store.setAccount(const Account(_server, 2, 'Bob'));
      store.claimPairingOwnership();
      container.read(pairedStateRevisionProvider.notifier).refresh();
      await tester.pumpAndSettle();

      await tester.tap(find.text('Save'));
      await tester.pumpAndSettle();

      expect(server.puts, isEmpty);
      expect(server.byToken['bob']!['timezone'], 'UTC');
      expect(find.text('Server timezone: UTC'), findsOneWidget);
    },
  );
}
