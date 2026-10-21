import 'package:drift/drift.dart' show Value, driftRuntimeOptions;
import 'package:drift/native.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:tracker/data/database.dart';
import 'package:tracker/reminders.dart';

const chore = '11111111-1111-4111-8111-111111111111';
const todo = '22222222-2222-4222-8222-222222222222';

Future<AppDb> seeded() async {
  final db = AppDb(NativeDatabase.memory());
  await db.upsertItem(ItemsCompanion.insert(id: chore, kind: 'chore', name: 'Recycling', points: 10, updatedAt: 1));
  await db.upsertItem(ItemsCompanion.insert(id: todo, kind: 'todo', name: 'Pay rent', points: 20, updatedAt: 1, dueDate: const Value('2026-10-19')));
  return db;
}

String payloadFor(String id, String date, [String kind = 'chore']) =>
    ReminderPayload(itemId: id, date: date, kind: kind).encode();

void main() {
  driftRuntimeOptions.dontWarnAboutMultipleDatabases = true;

  group('payload', () {
    test('parses the data the server sends and ignores test pushes and junk', () {
      final p = ReminderPayload.fromData({'item_id': chore, 'date': '2026-10-19', 'kind': 'chore', 'title': 'Recycling', 'body': 'Chore due now'});
      expect((p!.itemId, p.date, p.kind, p.title), (chore, '2026-10-19', 'chore', 'Recycling'));
      expect(ReminderPayload.fromData({'kind': 'test'}), isNull);
      expect(ReminderPayload.fromData({'item_id': chore, 'date': '2026-10-19', 'kind': 'test'}), isNull);
      expect(ReminderPayload.fromData({'item_id': '', 'date': 'x', 'kind': 'chore'}), isNull);
      expect(ReminderPayload.fromData({}), isNull);
    });

    test('round trips through the local notification payload and survives bad input', () {
      final enc = ReminderPayload(itemId: chore, date: '2026-10-19', kind: 'habit').encode();
      final back = ReminderPayload.decode(enc)!;
      expect((back.itemId, back.date, back.kind), (chore, '2026-10-19', 'habit'));
      expect(ReminderPayload.decode(null), isNull);
      expect(ReminderPayload.decode(''), isNull);
      expect(ReminderPayload.decode('not json'), isNull);
      expect(ReminderPayload.decode('[1,2]'), isNull);
    });

    test('notification id is stable per item and day, and differs across days', () {
      const a = ReminderPayload(itemId: chore, date: '2026-10-19', kind: 'chore');
      const b = ReminderPayload(itemId: chore, date: '2026-10-19', kind: 'chore');
      const c = ReminderPayload(itemId: chore, date: '2026-10-20', kind: 'chore');
      expect(a.notificationId, b.notificationId);
      expect(a.notificationId, isNot(c.notificationId));
      expect(a.notificationId, greaterThanOrEqualTo(0));
      expect(a.notificationId, lessThan(0x80000000));
      expect(ReminderPayload.decode(a.encode())!.notificationId, a.notificationId);
    });


    test('the notification carries a Mark done action that does not open the app', () {
      final a = reminderNotificationDetails().android!.actions!.single;
      expect((a.id, a.title, a.showsUserInterface), (markDoneActionId, 'Mark done', false));
    });
  });

  group('Mark done', () {
    test('writes a dirty completion for the reminded day, ready to sync', () async {
      final db = await seeded();
      final wrote = await handleReminderAction(markDoneActionId, payloadFor(chore, '2026-10-19'), db: db, nowMs: () => 5000);
      expect(wrote, isTrue);
      final c = await db.select(db.completions).getSingle();
      expect((c.itemId, c.date, c.deletedAt, c.dirty, c.updatedAt, c.completedAt), (chore, '2026-10-19', null, true, 5000, 5000));
    });

    test('tries to sync after writing, and a failing sync does not lose the completion', () async {
      final db = await seeded();
      var synced = 0;
      await handleReminderAction(markDoneActionId, payloadFor(chore, '2026-10-19'), db: db, sync: () async {
        synced++;
        throw Exception('offline');
      });
      expect(synced, 1);
      final c = await db.select(db.completions).getSingle();
      expect(c.dirty, isTrue, reason: 'still queued for the next sync');
    });

    test('a second tap, or an item already done that day, writes nothing more', () async {
      final db = await seeded();
      await db.setCompletion(chore, '2026-10-19', true, 1000);
      expect(await handleReminderAction(markDoneActionId, payloadFor(chore, '2026-10-19'), db: db, nowMs: () => 9000), isFalse);
      final c = await db.select(db.completions).getSingle();
      expect(c.updatedAt, 1000, reason: 'the original tick is untouched');
    });

    test('a chore done on another day is still marked for the reminded day', () async {
      final db = await seeded();
      await db.setCompletion(chore, '2026-10-18', true, 1000);
      expect(await handleReminderAction(markDoneActionId, payloadFor(chore, '2026-10-19'), db: db, nowMs: () => 9000), isTrue);
      expect(await db.select(db.completions).get(), hasLength(2));
    });

    test('a to-do ticked on any earlier day counts as done', () async {
      final db = await seeded();
      await db.setCompletion(todo, '2026-10-18', true, 1000);
      expect(await handleReminderAction(markDoneActionId, payloadFor(todo, '2026-10-19', 'todo'), db: db), isFalse);
      expect(await db.select(db.completions).get(), hasLength(1));
    });

    test('an un-ticked completion does not block marking done again', () async {
      final db = await seeded();
      await db.setCompletion(chore, '2026-10-19', true, 1000);
      await db.setCompletion(chore, '2026-10-19', false, 2000);
      expect(await handleReminderAction(markDoneActionId, payloadFor(chore, '2026-10-19'), db: db, nowMs: () => 3000), isTrue);
      final c = await db.select(db.completions).getSingle();
      expect((c.deletedAt, c.updatedAt), (null, 3000));
    });

    test('a deleted or unknown item, another action, or a bad payload writes nothing', () async {
      final db = await seeded();
      await db.softDeleteItem(chore, 10);
      expect(await handleReminderAction(markDoneActionId, payloadFor(chore, '2026-10-19'), db: db), isFalse);
      expect(await handleReminderAction(markDoneActionId, payloadFor('33333333-3333-4333-8333-333333333333', '2026-10-19'), db: db), isFalse);
      expect(await handleReminderAction('something_else', payloadFor(todo, '2026-10-19', 'todo'), db: db), isFalse);
      expect(await handleReminderAction(null, payloadFor(todo, '2026-10-19', 'todo'), db: db), isFalse);
      expect(await handleReminderAction(markDoneActionId, 'garbage', db: db), isFalse);
      expect(await db.select(db.completions).get(), isEmpty);
    });
  });
}
