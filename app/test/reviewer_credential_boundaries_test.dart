import 'dart:async';

import 'package:drift/native.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';
// ignore_for_file: depend_on_referenced_packages
import 'package:flutter_secure_storage_platform_interface/flutter_secure_storage_platform_interface.dart';
import 'package:flutter_secure_storage/test/test_flutter_secure_storage_platform.dart';
import 'package:tracker/data/api.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/data/store.dart';
import 'package:tracker/data/sync.dart';
import 'package:tracker/push.dart';

import 'push_test.dart' show FakeDevice;

class DelayedApi extends Api {
  DelayedApi(this.entered, this.release) : super('https://example.invalid');
  final Completer<void> entered, release;
  @override
  Future<void> setFcmToken(String? token) async {
    entered.complete();
    await release.future;
    throw AuthLost();
  }

  @override
  Future<Map<String, dynamic>> sync(
    int cursor,
    List<Map<String, dynamic>> items,
    List<Map<String, dynamic>> completions, {
    String? epoch,
  }) async {
    entered.complete();
    await release.future;
    throw AuthLost();
  }
}

class GatedSecure extends TestFlutterSecureStoragePlatform {
  GatedSecure() : super({'token': 'old'});
  bool armed = false;
  final readEntered = Completer<void>(), releaseRead = Completer<void>();
  @override
  Future<String?> read({
    required String key,
    required Map<String, String> options,
  }) async {
    final value = await super.read(key: key, options: options);
    if (armed) {
      armed = false;
      readEntered.complete();
      await releaseRead.future;
    }
    return value;
  }
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  for (final kind in ['push', 'sync']) {
    for (final sameToken in [false, true]) {
      test(
        '$kind stale401 preserves ${sameToken ? "same-token new generation" : "queued replacement write"}',
        () async {
          SharedPreferences.setMockInitialValues({});
          final secure = GatedSecure();
          FlutterSecureStoragePlatform.instance = secure;
          final store = Store(await SharedPreferences.getInstance());
          final entered = Completer<void>(), release = Completer<void>();
          final api = DelayedApi(entered, release);
          final db = AppDb(NativeDatabase.memory());
          addTearDown(db.close);
          final device = FakeDevice();
          final push = FcmPushService(
            store,
            device: device,
            apiFactory: (_, _) => api,
          );
          addTearDown(push.dispose);
          final engine = SyncEngine(db, store, apiFactory: (_, _) => api);
          final request = kind == 'push' ? push.register() : engine.sync();
          await entered.future;
          if (sameToken) {
            // Same token can belong to another server or a newly claimed pairing.
            final serverChange = kind == 'push';
            store.claimCredentialOwnership();
            engine.revalidate();
            if (serverChange) {
              await store.setServerUrl('https://new.example.invalid');
            }
            await store.saveToken('old');
            release.complete();
            await request;
            expect(await store.token, 'old');
          } else {
            final generation = store.authGeneration;
            secure.armed = true;
            final ownershipClaimed = Completer<void>();
            release.complete();
            await secure.readEntered.future.timeout(const Duration(seconds: 3));
            store.claimPairingOwnership();
            expect(generation + 1, store.authGeneration);
            engine.revalidate();
            ownershipClaimed.complete();
            final replacementWrite = store.saveToken('replacement');
            secure.releaseRead.complete();
            await request;
            await replacementWrite;
            expect(await store.token, 'replacement');
            expect(ownershipClaimed.isCompleted, isTrue);
          }
        },
      );
    }
  }
}
