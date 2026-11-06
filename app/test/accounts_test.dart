import 'dart:convert';
import 'dart:async';
import 'dart:io';

import 'package:drift/native.dart';
import 'package:drift/drift.dart' show Value;
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
// ignore: depend_on_referenced_packages
import 'package:flutter_secure_storage_platform_interface/flutter_secure_storage_platform_interface.dart';
import 'package:flutter_secure_storage/test/test_flutter_secure_storage_platform.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/data/sync.dart';
import 'package:tracker/state.dart';
import 'package:tracker/ui/pair.dart';
import 'package:tracker/ui/settings.dart';

class MemStore extends Store {
  MemStore(super.prefs, [String? token]) {
    FlutterSecureStoragePlatform.instance = TestFlutterSecureStoragePlatform({
      'token': ?token,
    });
  }
}

class RejectReplacementWrite extends TestFlutterSecureStoragePlatform {
  RejectReplacementWrite() : super({'token': 'old-token'});

  @override
  Future<void> write({
    required String key,
    required String value,
    required Map<String, String> options,
  }) async {
    if (value == 'rejected') {
      throw StateError('injected credential save failure');
    }
    await super.write(key: key, value: value, options: options);
  }
}

const itemA = '11111111-1111-4111-8111-111111111111';
const server = 'https://tracker.example.com';

/// A stand-in for the endpoints the app uses. Accounts are created from invite codes; every
/// request is recorded so tests can see what was pushed where.
class FakeServer {
  final calls = <String>[];
  final syncBodies = <Map<String, dynamic>>[];
  final invites = {'GOOD-CODE-0001': true};
  final names = {'admin'};
  int nextUser = 2;
  // token -> (user id, name)
  final tokens = <String, (int, String)>{'old-token': (1, 'admin')};
  bool meFails = false;
  String? lastTimezone;
  var pairCodes = 0;

  Future<http.Response> handle(http.Request r) async {
    calls.add('${r.method} ${r.url.path}');
    http.Response json(Object body, [int code = 200]) =>
        http.Response(jsonEncode(body), code);
    final body = r.body.isEmpty
        ? <String, dynamic>{}
        : jsonDecode(r.body) as Map<String, dynamic>;
    final auth = r.headers['authorization']?.replaceFirst('Bearer ', '');
    switch ('${r.method} ${r.url.path}') {
      case 'POST /signup':
        if (body['invite'] != 'GOOD-CODE-0001' ||
            invites['GOOD-CODE-0001'] != true) {
          return json({'error': 'unauthorized'}, 401);
        }
        final name = (body['name'] as String).trim();
        if (names.any((n) => n.toLowerCase() == name.toLowerCase())) {
          return json({'error': 'that name is already taken'}, 409);
        }
        invites['GOOD-CODE-0001'] = false;
        names.add(name);
        lastTimezone = body['timezone'] as String?;
        final id = nextUser++;
        final tok = 'tok-$id';
        tokens[tok] = (id, name);
        return json({
          'user_id': id,
          'name': name,
          'device_id': 'dev-$id',
          'token': tok,
        });
      case 'POST /pair':
        final code = body['code'] as String;
        if (!code.startsWith('PAIR')) {
          return json({'error': 'unauthorized'}, 401);
        }
        // PAIR-A... pairs as user 1, PAIR-B... as user 3
        final id = code.startsWith('PAIR-B') ? 3 : 1;
        final tok = 'tok-pair-$id-${calls.length}';
        tokens[tok] = (id, id == 1 ? 'admin' : 'Bob');
        return json({
          'user_id': id,
          'name': id == 1 ? 'admin' : 'Bob',
          'device_id': 'd',
          'token': tok,
        });
      case 'GET /me':
        if (meFails) return http.Response('<html>502</html>', 502);
        final t = tokens[auth];
        if (t == null) return json({'error': 'unauthorized'}, 401);
        return json({'user_id': t.$1, 'name': t.$2, 'is_admin': t.$1 == 1});
      case 'POST /sync':
        syncBodies.add(body);
        return json({
          'epoch': 'e1',
          'cursor': 5,
          'items': [],
          'completions': [],
          'server_time': 0,
        });
      case 'POST /pair-code':
        if (tokens[auth] == null) return json({'error': 'unauthorized'}, 401);
        pairCodes++;
        return json({'code': 'ABCD-EF$pairCodes', 'expires_in_secs': 900});
      case 'DELETE /device':
        return json({'ok': true});
    }
    return json({'ok': true});
  }
}

Future<(ProviderContainer, MemStore, AppDb, FakeServer)> setup({
  String? token,
  Account? account,
  bool withData = true,
  FakeServer? fake,
  Future<String?> Function()? tz,
}) async {
  SharedPreferences.setMockInitialValues({});
  final prefs = await SharedPreferences.getInstance();
  final store = MemStore(prefs, token);
  if (token != null) {
    await store.setServerUrl(server); // a paired phone knows its server
  }
  if (account != null) await store.setAccount(account);
  final db = AppDb(NativeDatabase.memory());
  if (withData) {
    await db.upsertItem(
      ItemsCompanion.insert(
        id: itemA,
        kind: 'chore',
        name: 'Old chore',
        points: 10,
        updatedAt: 1,
      ),
    );
    await db.setCompletion(itemA, '2026-10-19', true, 2);
  }
  final fs = fake ?? FakeServer();
  final client = MockClient(fs.handle);
  final c = ProviderContainer(
    overrides: [
      prefsProvider.overrideWithValue(prefs),
      storeProvider.overrideWithValue(store),
      dbProvider.overrideWithValue(db),
      apiFactoryProvider.overrideWithValue(
        (b, t) => Api(b, token: t, client: client),
      ),
      // The sync engine must talk to the fake too, or nothing it sends could be observed.
      syncEngineProvider.overrideWithValue(
        SyncEngine(
          db,
          store,
          apiFactory: (b, t) => Api(b, token: t, client: client),
        ),
      ),
      phoneTimezoneProvider.overrideWithValue(
        tz ?? () async => 'Europe/Berlin',
      ),
    ],
  );
  addTearDown(c.dispose);
  addTearDown(db.close);
  // Teardowns run last-in first: let the background sync and push registration that joining
  // starts finish before the container is disposed.
  addTearDown(() => Future<void>.delayed(const Duration(milliseconds: 150)));
  return (c, store, db, fs);
}

Future<List<ItemRow>> items(AppDb db) => db.select(db.items).get();

void main() {
  group('adopting an account', () {
    test('delayed old sync cannot restore private rows after account switch and reopen', () async {
      SharedPreferences.setMockInitialValues({});
      final prefs = await SharedPreferences.getInstance();
      final store = MemStore(prefs, 'old');
      await store.setServerUrl(server);
      await store.setAccount(const Account(server, 1, 'Alice'));
      final directory = await Directory('/tmp/opencode')
          .createTemp('track22-isolation-');
      final file = File('${directory.path}/local.db');
      var db = AppDb(NativeDatabase(file));
      final entered = Completer<void>();
      final release = Completer<http.Response>();
      final client = MockClient((request) async {
        if (request.headers['authorization'] == 'Bearer old') {
          entered.complete();
          return release.future;
        }
        return http.Response('{"cursor":0,"items":[],"completions":[]}', 200);
      });
      final container = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(prefs),
          storeProvider.overrideWithValue(store),
          dbProvider.overrideWithValue(db),
          syncEngineProvider.overrideWithValue(
            SyncEngine(
              db,
              store,
              apiFactory: (base, token) =>
                  Api(base, token: token, client: client),
            ),
          ),
        ],
      );
      final oldSync = container.read(controllerProvider).syncNow();
      await entered.future;
      await container
          .read(controllerProvider)
          .adoptAccount(
            server: server,
            token: 'new',
            account: const AccountInfo(userId: 2, name: 'Bob'),
          );
      release.complete(
        http.Response(
          jsonEncode({
            'cursor': 17,
            'items': [
              {
                'id': itemA,
                'kind': 'chore',
                'name': 'Alice private',
                'weekdays': 127,
                'remind_time': null,
                'points': 10,
                'due_date': null,
                'sort': 0,
                'active': true,
                'updated_at': 20,
                'deleted_at': null,
                'seq': 17,
              },
            ],
            'completions': [],
          }),
          200,
        ),
      );
      await oldSync;
      await Future<void>.delayed(const Duration(milliseconds: 50));
      container.dispose();
      await db.close();
      db = AppDb(NativeDatabase(file));
      expect(await db.select(db.items).get(), isEmpty);
      expect(await store.token, 'new');
      await db.close();
      client.close();
      await directory.delete(recursive: true);
    });
    test(
      'failed credential persistence never deletes the previous account rows',
      () async {
        final (container, store, db, _) = await setup(
          token: 'old-token',
          account: const Account(server, 1, 'Owner'),
        );
        FlutterSecureStoragePlatform.instance = RejectReplacementWrite();
        await expectLater(
          container
              .read(controllerProvider)
              .adoptAccount(
                server: 'https://replacement.invalid',
                token: 'rejected',
                account: const AccountInfo(userId: 2, name: 'Other'),
              ),
          throwsA(isA<PairingPersistenceFailure>()),
        );
        expect(await store.token, 'old-token');
        expect(store.serverUrl, server);
        expect(store.account!.userId, 1);
        expect(await items(db), hasLength(1));
        expect(await db.select(db.completions).get(), hasLength(1));
      },
    );
    test(
      'server tenant paths remain case-sensitive while host names do not',
      () {
        const account = Account('https://EXAMPLE.com/HouseholdA', 1, 'Owner');
        expect(account.sameAs('https://example.com/HouseholdA/', 1), isTrue);
        expect(account.sameAs('https://example.com/householda', 1), isFalse);
      },
    );

    test('failed legacy discovery followed by keep-data signout preserves unknown provenance', () async {
      final fake = FakeServer()..meFails = true;
      final (container, store, db, _) = await setup(
        token: 'old-token',
        fake: fake,
      );
      await container.read(controllerProvider).signOut();
      expect(await store.token, isNull);
      expect(store.prefs.getBool('account_owner_unknown'), isTrue);
      expect(
        await container.read(controllerProvider).localDataAtRisk(),
        isTrue,
      );
      await container
          .read(controllerProvider)
          .adoptAccount(
            server: server,
            token: 'replacement',
            account: const AccountInfo(userId: 9, name: 'Other'),
          );
      expect(await items(db), isEmpty);
    });
    test('a different user on the same server wipes the previous account\'s data before it can sync', () async {
      final (c, store, db, fs) = await setup(
        token: 'tok-2',
        account: const Account(server, 2, 'Sam'),
      );
      final wiped = await c
          .read(controllerProvider)
          .adoptAccount(
            server: server,
            token: 'tok-3',
            account: const AccountInfo(userId: 3, name: 'Bob', token: 'tok-3'),
          );
      expect(wiped, isTrue);
      expect(await items(db), isEmpty);
      expect(await db.select(db.completions).get(), isEmpty);
      expect(await store.token, 'tok-3');
      expect(store.account!.userId, 3);
      expect(store.account!.name, 'Bob');
      expect(store.cursor, 0);
      await Future<void>.delayed(const Duration(milliseconds: 50));
      expect(
        fs.syncBodies,
        isNotEmpty,
        reason: 'the new account did sync (otherwise this proves nothing)',
      );
      for (final b in fs.syncBodies) {
        expect(
          b['items'],
          isEmpty,
          reason: 'nothing of the old account is pushed to the new one',
        );
        expect(b['completions'], isEmpty);
      }
    });

    test('a different server wipes too, even for the same user id', () async {
      final (c, _, db, _) = await setup(
        token: 't',
        account: const Account('https://one.example.com', 1, 'a'),
      );
      final wiped = await c
          .read(controllerProvider)
          .adoptAccount(
            server: 'https://two.example.com',
            token: 'n',
            account: const AccountInfo(userId: 1, name: 'a'),
          );
      expect(wiped, isTrue);
      expect(await items(db), isEmpty);
    });

    test(
      'the same account again keeps its data and offers it all to the server',
      () async {
        final (c, store, db, fs) = await setup(
          token: 'old',
          account: const Account(server, 1, 'admin'),
        );
        await store.setCursor(40);
        await (db.update(db.items))
            .write(const ItemsCompanion(dirty: Value(false)));
        final wiped = await c
            .read(controllerProvider)
            .adoptAccount(
              server: '$server/', // trailing slash and case do not make it another server
              token: 'new',
              account: const AccountInfo(userId: 1, name: 'admin'),
            );
        expect(wiped, isFalse);
        expect(await items(db), hasLength(1));
        expect(store.cursor, 0);
        await Future<void>.delayed(const Duration(milliseconds: 50));
        expect(
          (fs.syncBodies.first['items'] as List),
          hasLength(1),
          reason: 'marked dirty and pushed',
        );
      },
    );

    test('a phone that was only ever used offline keeps its data and uploads it to the new account', () async {
      final (c, store, db, fs) = await setup();
      expect(
        await c.read(controllerProvider).localDataAtRisk(),
        isFalse,
        reason: 'no history, nothing to warn about',
      );
      final wiped = await c
          .read(controllerProvider)
          .adoptAccount(
            server: server,
            token: 'tok-9',
            account: const AccountInfo(userId: 9, name: 'New'),
          );
      expect(wiped, isFalse);
      expect(await items(db), hasLength(1));
      expect(store.account!.userId, 9);
      await Future<void>.delayed(const Duration(milliseconds: 50));
      expect((fs.syncBodies.first['items'] as List), hasLength(1));
    });

    test('a phone paired before accounts were recorded learns its owner, so re-pairing to the same account keeps data', () async {
      final (c, store, db, _) = await setup(
        token: 'old-token',
      ); // user 1 on the fake server, no account recorded
      expect(store.account, isNull);
      final wiped = await c
          .read(controllerProvider)
          .adoptAccount(
            server: server,
            token: 'tok-again',
            account: const AccountInfo(userId: 1, name: 'admin'),
          );
      expect(
        wiped,
        isFalse,
        reason: 'the server said the old token was user 1, the same account',
      );
      expect(await items(db), hasLength(1));
    });

    test('a pre-accounts phone that cannot reach the server errs on the side of not mixing', () async {
      final fake = FakeServer()..meFails = true;
      final (c, _, db, _) = await setup(token: 'old-token', fake: fake);
      expect(await c.read(controllerProvider).localDataAtRisk(), isTrue);
      final wiped = await c
          .read(controllerProvider)
          .adoptAccount(
            server: server,
            token: 'tok-9',
            account: const AccountInfo(userId: 9, name: 'Other'),
          );
      expect(
        wiped,
        isTrue,
        reason: 'owner unknown, so the data is not trusted to belong to the new account',
      );
      expect(await items(db), isEmpty);
    });

    test('no data means no warning even for a signed-in phone', () async {
      final (c, _, _, _) = await setup(
        token: 'x',
        account: const Account(server, 2, 'Sam'),
        withData: false,
      );
      expect(await c.read(controllerProvider).localDataAtRisk(), isFalse);
    });

    test('a signed-in phone with data is warned', () async {
      final (c, _, _, _) = await setup(
        token: 'x',
        account: const Account(server, 2, 'Sam'),
      );
      expect(await c.read(controllerProvider).localDataAtRisk(), isTrue);
    });
  });

  group('sign out', () {
    test('keeps the account so the data stays attributed; wiping forgets it', () async {
      final (c, store, db, _) = await setup(token: 'old-token');
      expect(await c.read(controllerProvider).signOut(), isTrue);
      expect(await store.token, isNull);
      expect(store.prefs.getBool('account_owner_unknown'), isTrue);
      expect(
        store.account,
        isNull,
        reason: 'signout preserves unknown provenance without a new network request',
      );
      expect(await items(db), hasLength(1));

      // Without a confirmed owner, a new token cannot claim retained rows.
      final wiped = await c
          .read(controllerProvider)
          .adoptAccount(
            server: server,
            token: 'back',
            account: const AccountInfo(userId: 1, name: 'admin'),
          );
      expect(
        wiped,
        isTrue,
        reason:
            'an unknown retained owner cannot be assumed to match a new token',
      );
      expect(await items(db), isEmpty);
    });

    test('keeping data and then joining as someone else wipes it', () async {
      final (c, _, db, _) = await setup(token: 'old-token');
      await c.read(controllerProvider).signOut();
      final wiped = await c
          .read(controllerProvider)
          .adoptAccount(
            server: server,
            token: 'tok-7',
            account: const AccountInfo(userId: 7, name: 'Else'),
          );
      expect(wiped, isTrue);
      expect(await items(db), isEmpty);
    });

    test('sign out and delete forgets the account too, so the next phone user starts clean', () async {
      final (c, store, db, _) = await setup(token: 'old-token');
      await c.read(controllerProvider).signOut(wipeLocal: true);
      expect(await items(db), isEmpty);
      expect(store.account, isNull);
      expect(await c.read(controllerProvider).localDataAtRisk(), isFalse);
    });
  });

  group('API client', () {
    test('signup sends the invite, name and the phone timezone, and returns the account', () async {
      final fs = FakeServer();
      final api = Api(server, client: MockClient(fs.handle));
      final a = await api.signup(
        invite: 'GOOD-CODE-0001',
        name: 'Riley',
        deviceName: 'Pixel',
        timezone: 'Europe/Berlin',
      );
      expect((a.userId, a.name, a.token), (2, 'Riley', 'tok-2'));
      expect(fs.lastTimezone, 'Europe/Berlin');
    });

    test(
      'an old server that does not report the account is reported clearly',
      () async {
        final api = Api(
          server,
          client: MockClient(
            (_) async => http.Response('{"token":"t","device_id":"d"}', 200),
          ),
        );
        await expectLater(
          api.pair('PAIR-A', 'x'),
          throwsA(
            isA<ApiException>().having(
              (e) => e.message,
              'message',
              contains('too old'),
            ),
          ),
        );
      },
    );
  });

  group('screens', () {
    Future<ProviderContainer> pumpPair(
      WidgetTester tester, {
      String? token,
      Account? account,
      bool withData = true,
      FakeServer? fake,
    }) async {
      final (c, _, _, _) = await setup(
        token: token,
        account: account,
        withData: withData,
        fake: fake,
      );
      tester.view.physicalSize = const Size(1080, 2400);
      tester.view.devicePixelRatio = 2.625;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: c,
          child: const MaterialApp(home: PairScreen()),
        ),
      );
      await tester.pumpAndSettle();
      return c;
    }

    Future<void> fill(
      WidgetTester tester, {
      String? name,
      required String code,
    }) async {
      if (name != null) {
        await tester.enterText(
          find.widgetWithText(TextField, 'Your name'),
          name,
        );
      }
      await tester.enterText(find.byType(TextField).last, code);
      await tester.pump();
    }

    testWidgets(
      'join with an invite creates the account, sends the timezone and signs in',
      (tester) async {
        final fs = FakeServer();
        final c = await pumpPair(tester, fake: fs, withData: false);
        expect(find.text('Your name'), findsOneWidget);
        await fill(tester, name: 'Riley', code: 'GOOD-CODE-0001');
        await tester.tap(find.widgetWithText(FilledButton, 'Join'));
        await tester.pumpAndSettle();
        expect(fs.calls, contains('POST /signup'));
        expect(fs.lastTimezone, 'Europe/Berlin');
        final store = c.read(storeProvider);
        expect(await store.token, 'tok-2');
        expect(store.account!.name, 'Riley');
        expect(
          syncLabel(store, true),
          'Signed in as Riley on ${store.serverUrl}',
        );
      },
    );

    testWidgets(
      'join errors are explained: bad or used invite, taken name, missing fields',
      (tester) async {
        final fs = FakeServer();
        await pumpPair(tester, fake: fs, withData: false);

        await tester.tap(find.widgetWithText(FilledButton, 'Join'));
        await tester.pump();
        expect(find.text('Choose a name for your account.'), findsOneWidget);
        expect(fs.calls, isEmpty, reason: 'nothing is sent for an empty form');

        await fill(tester, name: 'Riley', code: 'WRONG-CODE-0000');
        await tester.tap(find.widgetWithText(FilledButton, 'Join'));
        await tester.pumpAndSettle();
        expect(
          find.textContaining('wrong, already used, or has expired'),
          findsOneWidget,
        );

        await fill(tester, name: 'Admin', code: 'GOOD-CODE-0001');
        await tester.tap(find.widgetWithText(FilledButton, 'Join'));
        await tester.pumpAndSettle();
        expect(
          find.text('That name is already taken. Choose another.'),
          findsOneWidget,
        );
        expect(
          fs.invites['GOOD-CODE-0001'],
          isTrue,
          reason: 'the taken name did not spend the invite',
        );

        await fill(tester, code: 'short');
        await tester.tap(find.widgetWithText(FilledButton, 'Join'));
        await tester.pump();
        expect(
          find.text('Enter the invite code you were given.'),
          findsOneWidget,
        );
      },
    );

    testWidgets('the unreachable server is a clear message, not a crash', (
      tester,
    ) async {
      final (c, _, _, _) = await setup(withData: false);
      final c2 = ProviderContainer(
        overrides: [
          prefsProvider.overrideWithValue(c.read(prefsProvider)),
          storeProvider.overrideWithValue(c.read(storeProvider)),
          dbProvider.overrideWithValue(c.read(dbProvider)),
          apiFactoryProvider.overrideWithValue(
            (b, t) => Api(
              b,
              token: t,
              client: MockClient(
                (_) async => throw http.ClientException('offline'),
              ),
            ),
          ),
          phoneTimezoneProvider.overrideWithValue(() async => null),
        ],
      );
      addTearDown(c2.dispose);
      tester.view.physicalSize = const Size(1080, 2400);
      tester.view.devicePixelRatio = 2.625;
      addTearDown(tester.view.reset);
      await tester.pumpWidget(
        UncontrolledProviderScope(
          container: c2,
          child: const MaterialApp(home: PairScreen()),
        ),
      );
      await tester.pumpAndSettle();
      await fill(tester, name: 'Riley', code: 'GOOD-CODE-0001');
      await tester.tap(find.widgetWithText(FilledButton, 'Join'));
      await tester.pumpAndSettle();
      expect(find.text('Cannot reach the server'), findsOneWidget);
    });

    testWidgets(
      'joining while the phone holds another account\'s data asks first, and Cancel changes nothing',
      (tester) async {
        final fs = FakeServer();
        final c = await pumpPair(
          tester,
          fake: fs,
          token: 'tok-5',
          account: const Account(server, 5, 'Sam'),
        );
        await fill(tester, name: 'Riley', code: 'GOOD-CODE-0001');
        await tester.tap(find.widgetWithText(FilledButton, 'Join'));
        await tester.pumpAndSettle();
        expect(find.text('Replace the data on this phone?'), findsOneWidget);
        expect(find.textContaining('never mix'), findsOneWidget);

        await tester.tap(find.text('Cancel'));
        await tester.pumpAndSettle();
        expect(
          fs.calls.where((c) => c == 'POST /signup'),
          isEmpty,
          reason: 'declining sends nothing',
        );
        expect(
          await c.read(dbProvider).select(c.read(dbProvider).items).get(),
          hasLength(1),
        );
        expect(await c.read(storeProvider).token, 'tok-5');

        await tester.tap(find.widgetWithText(FilledButton, 'Join'));
        await tester.pumpAndSettle();
        await tester.tap(find.text('Continue'));
        await tester.pumpAndSettle();
        expect(fs.calls, contains('POST /signup'));
        expect(
          await c.read(dbProvider).select(c.read(dbProvider).items).get(),
          isEmpty,
        );
        expect(c.read(storeProvider).account!.name, 'Riley');
      },
    );

    testWidgets(
      'pairing with a code for another account asks first and then replaces the data',
      (tester) async {
        final fs = FakeServer();
        final c = await pumpPair(
          tester,
          fake: fs,
          token: 'tok-2',
          account: const Account(server, 2, 'Sam'),
        );
        await tester.tap(find.text('Pair with a code'));
        await tester.pumpAndSettle();
        expect(
          find.text('Your name'),
          findsNothing,
          reason: 'pairing needs no name',
        );
        await fill(tester, code: 'PAIR-B123');
        await tester.tap(find.widgetWithText(FilledButton, 'Pair'));
        await tester.pumpAndSettle();
        expect(find.text('Replace the data on this phone?'), findsOneWidget);
        await tester.tap(find.text('Continue'));
        await tester.pumpAndSettle();
        expect(c.read(storeProvider).account!.userId, 3);
        expect(
          await c.read(dbProvider).select(c.read(dbProvider).items).get(),
          isEmpty,
        );
      },
    );

    testWidgets('pairing a code for the same account keeps the data', (
      tester,
    ) async {
      final fs = FakeServer();
      final c = await pumpPair(
        tester,
        fake: fs,
        token: 'old-token',
        account: const Account(server, 1, 'admin'),
      );
      await tester.tap(find.text('Pair with a code'));
      await tester.pumpAndSettle();
      await fill(tester, code: 'PAIR-A123');
      await tester.tap(find.widgetWithText(FilledButton, 'Pair'));
      await tester.pumpAndSettle();
      await tester.tap(
        find.text('Continue'),
      ); // it warns because it cannot know in advance
      await tester.pumpAndSettle();
      expect(c.read(storeProvider).account!.userId, 1);
      expect(
        await c.read(dbProvider).select(c.read(dbProvider).items).get(),
        hasLength(1),
        reason: 'same account, nothing deleted',
      );
    });

    testWidgets(
      'Settings shows who you are and "Add another device" shows a code',
      (tester) async {
        final fs = FakeServer();
        final (c, store, _, _) = await setup(
          token: 'old-token',
          account: const Account(server, 1, 'admin'),
          fake: fs,
        );
        tester.view.physicalSize = const Size(1080, 2600);
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
        expect(
          find.text('Signed in as admin on ${store.serverUrl}'),
          findsOneWidget,
        );
        await tester.ensureVisible(find.text('Add another device'));
        await tester.tap(find.text('Add another device'));
        await tester.pumpAndSettle();
        expect(fs.calls, contains('POST /pair-code'));
        expect(find.text('ABCD-EF1'), findsOneWidget);
        expect(find.textContaining('Single use'), findsOneWidget);
        await tester.tap(find.text('New code'));
        await tester.pumpAndSettle();
        expect(find.text('ABCD-EF2'), findsOneWidget);
      },
    );
  });
}
