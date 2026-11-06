import 'package:drift/native.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/sync.dart';

import 'sync_test.dart' show MemStore;

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  for (final status in [403, 422, 429]) {
    test('reachable policy error $status is not reported as offline', () async {
      SharedPreferences.setMockInitialValues({});
      final store = MemStore(await SharedPreferences.getInstance());
      final db = AppDb(NativeDatabase.memory());
      addTearDown(db.close);
      final client = MockClient(
        (_) async => http.Response('{"error":"policy-$status"}', status),
      );
      addTearDown(client.close);
      final sync = SyncEngine(
        db,
        store,
        apiFactory: (base, token) => Api(base, token: token, client: client),
      );
      expect(await sync.sync(), SyncStatus.rejected);
      expect(sync.rejectionMessage, 'policy-$status');
      expect(await store.token, 'tok');
    });
  }
}
