import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:tracker/state.dart';
import 'package:tracker/data/sync.dart';

import 'signout_test.dart' as fixture;

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  test(
    'sync started during delayed revoke cannot restore private rows after wipe',
    () async {
      final deleting = Completer<void>(), releaseDelete = Completer<void>();
      final syncing = Completer<void>();
      final (c, store, db) = await fixture.setup(
        MockClient((r) async {
          if (r.method == 'DELETE') {
            deleting.complete();
            await releaseDelete.future;
            return http.Response('{}', 200);
          }
          syncing.complete();
          return http.Response(
            '{"cursor":17,"items":[{"id":"11111111-1111-4111-8111-111111111111","kind":"chore","name":"private restored","weekdays":127,"remind_time":null,"points":10,"due_date":null,"sort":0,"active":true,"updated_at":1,"deleted_at":null,"seq":17}],"completions":[]}',
            200,
          );
        }),
      );
      addTearDown(db.close);
      final signout = c.read(controllerProvider).signOut(wipeLocal: true);
      await deleting.future;
      await Future<void>.delayed(Duration.zero);
      final sync = c.read(syncEngineProvider).sync();
      expect(await sync, SyncStatus.unpaired);
      expect(syncing.isCompleted, isFalse);
      releaseDelete.complete();
      await signout;
      expect(await db.select(db.items).get(), isEmpty);
      expect(
        syncing.isCompleted,
        isFalse,
        reason: 'invalidated engine must not admit old-session request',
      );
      expect(await db.select(db.items).get(), isEmpty);
      expect(store.cursor, 0);
    },
  );
}
