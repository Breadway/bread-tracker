import 'package:drift/drift.dart' show Value;
import 'package:drift/native.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/sync.dart';

import 'sync_test.dart' show MemStore, id1;

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  for (final (code, expected) in [
    (403, SyncStatus.accountDisabled),
    (422, SyncStatus.quotaExhausted),
    (429, SyncStatus.rateLimited),
    (400, SyncStatus.rejected),
  ]) {
    test(
      'reachable policy error $code gets a typed status and keeps dirty data',
      () async {
        SharedPreferences.setMockInitialValues({});
        final store = MemStore(await SharedPreferences.getInstance());
        final db = AppDb(NativeDatabase.memory());
        addTearDown(db.close);
        await db.upsertItem(
          ItemsCompanion.insert(
            id: id1,
            kind: 'habit',
            name: 'Read',
            points: 15,
            updatedAt: 1,
            dirty: const Value(true),
          ),
        );
        var refuse = true;
        final client = MockClient((_) async {
          if (refuse) return http.Response('{"error":"policy-$code"}', code);
          return http.Response(
            '{"cursor":0,"items":[],"completions":[],"server_time":0}',
            200,
          );
        });
        addTearDown(client.close);
        final sync = SyncEngine(
          db,
          store,
          apiFactory: (base, token) => Api(base, token: token, client: client),
        );

        expect(await sync.sync(), expected);
        expect(sync.rejectionMessage, 'policy-$code');
        expect(
          expected,
          isNot(SyncStatus.offline),
          reason: 'the server answered; this is not a network outage',
        );
        expect(await store.token, 'tok', reason: 'the credential is kept');
        expect(
          await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(),
          hasLength(1),
          reason: 'the dirty row is kept for a later retry',
        );

        // Once the policy refusal is gone, an ordinary retry drains the same
        // dirty row instead of requiring a new pairing or a local edit.
        refuse = false;
        expect(await sync.sync(), SyncStatus.idle);
        expect(
          await (db.select(db.items)..where((t) => t.dirty.equals(true))).get(),
          isEmpty,
        );
        expect(await store.token, 'tok');
      },
    );
  }
}
