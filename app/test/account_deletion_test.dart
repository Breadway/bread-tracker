import 'dart:convert';
import 'dart:async';
import 'dart:io';

import 'package:drift/native.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:flutter_secure_storage/test/test_flutter_secure_storage_platform.dart';
// ignore: depend_on_referenced_packages
import 'package:flutter_secure_storage_platform_interface/flutter_secure_storage_platform_interface.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/data/sync.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/settings.dart';

class MemStore extends Store {
  MemStore(super.prefs, String? token) {
    FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({
      'token': ?token,
    });
  }
}

const itemA = '11111111-1111-4111-8111-111111111111';
const server = 'https://tracker.example.com';

class Fake {
  final calls = <String>[];
  Object? deleteBody;
  int deleteStatus = 200;
  String deleteError = '';
  Completer<void>? deletionStarted;
  Completer<void>? releaseDeletion;
  Map<String, dynamic> exportDoc = {
    'format': 'bread-tracker-export',
    'version': 1,
    'items': [],
  };

  Future<http.Response> handle(http.Request r) async {
    calls.add('${r.method} ${r.url.path}');
    switch ('${r.method} ${r.url.path}') {
      case 'DELETE /account':
        deleteBody = jsonDecode(r.body);
        deletionStarted?.complete();
        if (releaseDeletion != null) await releaseDeletion!.future;
        if (deleteStatus != 200) {
          return http.Response(
            jsonEncode({'error': deleteError}),
            deleteStatus,
          );
        }
        return http.Response('{"deleted":true}', 200);
      case 'GET /export':
        return http.Response(jsonEncode(exportDoc), 200);
      case 'GET /me':
        return http.Response(
          '{"user_id":2,"name":"Sam","is_admin":false}',
          200,
        );
    }
    return http.Response('{}', 200);
  }
}

Future<(ProviderContainer, MemStore, AppDb, Fake)> setup({
  String? token = 'tok',
  File? databaseFile,
}) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final store = MemStore(prefs, token);
  await store.setServerUrl(server);
  await store.setAccount(const Account(server, 2, 'Sam'));
  await store.setCursor(9);
  final db = AppDb(
    databaseFile == null
        ? NativeDatabase.memory()
        : NativeDatabase(databaseFile),
  );
  await db.upsertItem(
    ItemsCompanion.insert(
      id: itemA,
      kind: 'chore',
      name: 'Chore',
      points: 10,
      updatedAt: 1,
    ),
  );
  await db.setCompletion(itemA, '2026-10-19', true, 2);
  final fake = Fake();
  final client = MockClient(fake.handle);
  final c = ProviderContainer(
    overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(store),
      dbProvider.overrideWithValue(db),
      apiFactoryProvider.overrideWithValue(
        (b, t) => Api(b, token: t, client: client),
      ),
      syncEngineProvider.overrideWithValue(
        SyncEngine(
          db,
          store,
          apiFactory: (b, t) => Api(b, token: t, client: client),
        ),
      ),
    ],
  );
  addTearDown(c.dispose);
  addTearDown(db.close);
  addTearDown(() => Future<void>.delayed(const Duration(milliseconds: 100)));
  return (c, store, db, fake);
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  test(
    'delayed account deletion cannot clear replacement credentials or rows',
    () async {
      final directory = await Directory.systemTemp.createTemp(
        'track26-delete-',
      );
      addTearDown(() async {
        if (directory.existsSync()) await directory.delete(recursive: true);
      });
      final databaseFile = File('${directory.path}/local.db');
      final (container, store, db, fake) = await setup(
        databaseFile: databaseFile,
      );
      fake.deletionStarted = Completer<void>();
      fake.releaseDeletion = Completer<void>();
      final oldDeletion = container
          .read(controllerProvider)
          .deleteAccount('Sam');
      await fake.deletionStarted!.future;
      final controller = container.read(controllerProvider);
      await controller.pairedAgain();
      await store.persistPairing(
        store.pairingOwnership('https://replacement.invalid'),
        'https://replacement.invalid',
        'replacement-token',
      );
      await store.setAccount(
        const Account('https://replacement.invalid', 3, 'Other'),
      );
      controller.pairingPersisted();
      fake.releaseDeletion!.complete();
      await oldDeletion;
      expect(await store.token, 'replacement-token');
      expect(store.account!.userId, 3);
      expect(await db.select(db.items).get(), hasLength(1));
      expect(await db.select(db.completions).get(), hasLength(1));
      await db.close();
      final reopened = AppDb(NativeDatabase(databaseFile));
      expect(await reopened.select(reopened.items).get(), hasLength(1));
      expect(await reopened.select(reopened.completions).get(), hasLength(1));
      await reopened.close();
    },
  );
  test(
    'deleting the account sends the name, then forgets everything on the phone',
    () async {
      final (c, store, db, fake) = await setup();
      await c.read(controllerProvider).deleteAccount('Sam');
      expect(fake.deleteBody, {'confirm': 'Sam'});
      expect(await store.token, isNull);
      expect(store.account, isNull);
      expect(store.cursor, 0);
      expect(await db.select(db.items).get(), isEmpty);
      expect(await db.select(db.completions).get(), isEmpty);
      expect(
        await c.read(controllerProvider).localDataAtRisk(),
        isFalse,
        reason: 'no history is left to protect',
      );
    },
  );

  for (final (status, message) in [
    (403, 'the admin account cannot be deleted from the app'),
    (400, 'confirmation does not match the account name'),
    (422, 'not allowed'),
  ]) {
    test(
      'a refused deletion ($status) leaves the phone exactly as it was',
      () async {
        final (c, store, db, fake) = await setup();
        fake.deleteStatus = status;
        fake.deleteError = message;
        await expectLater(
          c.read(controllerProvider).deleteAccount('Sam'),
          throwsA(
            isA<ApiException>().having((e) => e.message, 'message', message),
          ),
        );
        expect(await store.token, 'tok');
        expect(store.account?.userId, 2);
        expect(store.cursor, 9);
        expect(await db.select(db.items).get(), hasLength(1));
        expect(await db.select(db.completions).get(), hasLength(1));
      },
    );
  }

  test('an unreachable server leaves the phone as it was', () async {
    final (c, store, db, _) = await setup();
    final offline = ProviderContainer(
      overrides: [
        prefsProvider.overrideWithValue(c.read(prefsProvider)),
        storeProvider.overrideWithValue(store),
        dbProvider.overrideWithValue(db),
        apiFactoryProvider.overrideWithValue(
          (b, t) => Api(
            b,
            token: t,
            client: MockClient(
              (_) async => throw http.ClientException('offline'),
            ),
          ),
        ),
      ],
    );
    addTearDown(offline.dispose);
    await expectLater(
      offline.read(controllerProvider).deleteAccount('Sam'),
      throwsA(isA<ApiException>()),
    );
    expect(await store.token, 'tok');
    expect(await db.select(db.items).get(), hasLength(1));
  });

  test(
    'a revoked token during deletion is reported and keeps the data',
    () async {
      final (c, store, db, fake) = await setup();
      fake.deleteStatus = 401;
      await expectLater(
        c.read(controllerProvider).deleteAccount('Sam'),
        throwsA(isA<AuthLost>()),
      );
      expect(await db.select(db.items).get(), hasLength(1));
      expect(await store.token, 'tok');
    },
  );

  test('export returns the server document as readable JSON', () async {
    final (c, _, _, fake) = await setup();
    fake.exportDoc = {
      'format': 'bread-tracker-export',
      'version': 1,
      'items': [
        {'id': itemA, 'name': 'Chore'},
      ],
    };
    final text = await c.read(controllerProvider).exportJson();
    expect(text, contains('\n'), reason: 'pretty printed');
    expect(jsonDecode(text), fake.exportDoc);
    expect(fake.calls, contains('GET /export'));
  });

  group('screen', () {
    testWidgets(
      'large export uses an in-app file sharing path without truncation',
      (tester) async {
        final (original, store, db, fake) = await setup();
        fake.exportDoc = {
          'format': 'bread-tracker-export',
          'items': [
            {'name': List.filled(600001, 'x').join()},
          ],
        };
        String? shared;
        final client = MockClient(fake.handle);
        addTearDown(client.close);
        final container = ProviderContainer(
          overrides: [
            prefsProvider.overrideWithValue(original.read(prefsProvider)),
            storeProvider.overrideWithValue(store),
            dbProvider.overrideWithValue(db),
            apiFactoryProvider.overrideWithValue(
              (base, token) => Api(base, token: token, client: client),
            ),
            exportShareProvider.overrideWithValue((json) async {
              shared = json;
            }),
          ],
        );
        addTearDown(container.dispose);
        await tester.pumpWidget(
          UncontrolledProviderScope(
            container: container,
            child: const MaterialApp(home: Scaffold(body: SettingsScreen())),
          ),
        );
        await tester.pumpAndSettle();
        await tester.ensureVisible(find.text('Export my data'));
        await tester.tap(find.text('Export my data'));
        await tester.pumpAndSettle();
        expect(shared, isNotNull);
        expect(jsonDecode(shared!), fake.exportDoc);
        expect(find.textContaining('too large'), findsNothing);
        expect(tester.takeException(), isNull);
      },
    );
    Future<(ProviderContainer, Fake, List<String>)> pumpSettings(
      WidgetTester tester,
    ) async {
      final (c, _, _, fake) = await setup();
      final clip = <String>[];
      tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        (call) async {
          if (call.method == 'Clipboard.setData') {
            clip.add((call.arguments as Map)['text'] as String);
          }
          return null;
        },
      );
      addTearDown(
        () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
          SystemChannels.platform,
          null,
        ),
      );
      tester.view.physicalSize = const Size(1080, 3200);
      tester.view.devicePixelRatio = 2.625;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: c,
          child: MaterialApp(
            home: Scaffold(body: SettingsScreen(key: UniqueKey())),
          ),
        ),
      );
      await tester.pumpAndSettle();
      return (c, fake, clip);
    }

    testWidgets('Export my data copies the JSON to the clipboard', (
      tester,
    ) async {
      final (_, fake, clip) = await pumpSettings(tester);
      await tester.ensureVisible(find.text('Export my data'));
      await tester.tap(find.text('Export my data'));
      await tester.pumpAndSettle();
      expect(fake.calls, contains('GET /export'));
      expect(clip, hasLength(1));
      expect(jsonDecode(clip.single)['format'], 'bread-tracker-export');
      expect(find.textContaining('Copied'), findsOneWidget);
    });

    testWidgets(
      'Delete my account stays disabled until the name is typed, Cancel does nothing',
      (tester) async {
        final (c, fake, _) = await pumpSettings(tester);
        await tester.ensureVisible(find.text('Delete my account'));
        await tester.tap(find.text('Delete my account'));
        await tester.pumpAndSettle();
        expect(find.textContaining('cannot be undone'), findsOneWidget);
        FilledButton button() => tester.widget<FilledButton>(
          find.widgetWithText(FilledButton, 'Delete everything'),
        );
        expect(button().onPressed, isNull);
        await tester.enterText(find.byType(TextField), 'Sa');
        await tester.pump();
        expect(
          button().onPressed,
          isNull,
          reason: 'a partial name is not enough',
        );
        await tester.tap(find.text('Cancel'));
        await tester.pumpAndSettle();
        expect(fake.calls.where((x) => x == 'DELETE /account'), isEmpty);
        expect(await c.read(storeProvider).token, 'tok');
      },
    );

    testWidgets(
      'typing the name deletes the account and the phone is cleaned up',
      (tester) async {
        final (c, fake, _) = await pumpSettings(tester);
        await tester.ensureVisible(find.text('Delete my account'));
        await tester.tap(find.text('Delete my account'));
        await tester.pumpAndSettle();
        await tester.enterText(find.byType(TextField), 'sam');
        await tester.pump();
        await tester.tap(find.text('Delete everything'));
        await tester.pumpAndSettle();
        expect(fake.deleteBody, {'confirm': 'sam'});
        expect(await c.read(storeProvider).token, isNull);
        expect(
          await c.read(dbProvider).select(c.read(dbProvider).items).get(),
          isEmpty,
        );
        expect(find.text('Your account was deleted'), findsOneWidget);
      },
    );

    testWidgets('a server refusal is shown and nothing is deleted locally', (
      tester,
    ) async {
      final (c, fake, _) = await pumpSettings(tester);
      fake.deleteStatus = 403;
      fake.deleteError = 'the admin account cannot be deleted from the app';
      await tester.ensureVisible(find.text('Delete my account'));
      await tester.tap(find.text('Delete my account'));
      await tester.pumpAndSettle();
      await tester.enterText(find.byType(TextField), 'Sam');
      await tester.pump();
      await tester.tap(find.text('Delete everything'));
      await tester.pumpAndSettle();
      expect(
        find.textContaining('admin account cannot be deleted'),
        findsOneWidget,
      );
      expect(await c.read(storeProvider).token, 'tok');
      expect(
        await c.read(dbProvider).select(c.read(dbProvider).items).get(),
        hasLength(1),
      );
    });
  });
}
