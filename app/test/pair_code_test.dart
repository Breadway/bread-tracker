// ignore_for_file: depend_on_referenced_packages
import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_secure_storage/test/test_flutter_secure_storage_platform.dart';
import 'package:flutter_secure_storage_platform_interface/flutter_secure_storage_platform_interface.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/push.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/settings.dart';

void main() {
  for (final delayed in [false, true]) {
    testWidgets(
      'own-device code dialog ${delayed ? 'rejects stale response' : 'shows expiry and refreshes'}',
      (tester) async {
        SharedPreferences.setMockInitialValues({
          'server': 'https://old.invalid',
        });
        FlutterSecureStoragePlatform.instance =
            TestFlutterSecureStoragePlatform({'token': 'tok'});
        final prefs = await SharedPreferences.getInstance();
        final store = Store(prefs);
        final gate = Completer<void>();
        var calls = 0;
        final client = MockClient((request) async {
          expect(request.method, 'POST');
          expect(request.url.path, '/pair-code');
          expect(request.headers['authorization'], 'Bearer tok');
          calls++;
          if (delayed) await gate.future;
          return http.Response(
            jsonEncode({'code': 'CODE-$calls', 'expires_in_secs': 900}),
            200,
          );
        });
        final container = ProviderContainer(
          overrides: [
            prefsProvider.overrideWithValue(prefs),
            storeProvider.overrideWithValue(store),
            pushProvider.overrideWithValue(NoPushService()),
            apiFactoryProvider.overrideWithValue(
              (server, token) => Api(server, token: token, client: client),
            ),
          ],
        );
        addTearDown(container.dispose);
        addTearDown(client.close);
        await tester.pumpWidget(
          UncontrolledProviderScope(
            container: container,
            child: const MaterialApp(home: Scaffold(body: SettingsScreen())),
          ),
        );
        await tester.pumpAndSettle();
        await tester.ensureVisible(find.text('Add another device'));
        await tester.tap(find.text('Add another device'));
        if (delayed) {
          await tester.pump();
          store.claimPairingOwnership();
          await store.setServerUrl('https://replacement.invalid');
          gate.complete();
          await tester.pumpAndSettle();
          expect(find.text('CODE-1'), findsNothing);
        } else {
          await tester.pumpAndSettle();
          expect(find.text('CODE-1'), findsOneWidget);
          expect(find.textContaining('Single use'), findsOneWidget);
          await tester.tap(find.text('New code'));
          await tester.pumpAndSettle();
          expect(find.text('CODE-2'), findsOneWidget);
        }
        expect(tester.takeException(), isNull);
      },
    );
  }
}
