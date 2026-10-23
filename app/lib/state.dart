import 'dart:async';

import 'dart:ui';

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:shared_preferences/shared_preferences.dart';
import 'package:uuid/uuid.dart';
import 'package:drift/drift.dart' show Value;

import 'data/api.dart';
import 'data/database.dart';
import 'data/store.dart';
import 'data/sync.dart';
import 'fx/fx.dart';
import 'fx/sfx.dart';
import 'push.dart';
import 'rules.dart';
import 'theme.dart';

final prefsProvider = Provider<SharedPreferences>(
  (ref) => throw UnimplementedError('override in main'),
);
final storeProvider = Provider<Store>((ref) => Store(ref.watch(prefsProvider)));
final dbProvider = Provider<AppDb>((ref) {
  final db = AppDb();
  ref.onDispose(db.close);
  return db;
});
final syncEngineProvider = Provider<SyncEngine>(
  (ref) => SyncEngine(ref.watch(dbProvider), ref.watch(storeProvider)),
);
/// How the app builds an [Api] for one-off calls (settings, sign out). Tests override it.
final apiFactoryProvider = Provider<Api Function(String base, String token)>(
  (ref) => (base, token) => Api(base, token: token),
);
final sfxProvider = Provider<Sfx>((ref) => NoSfx());
final pushProvider = Provider<PushService>((ref) => NoPushService());
final fxProvider = Provider<FxController>((ref) {
  final c = FxController();
  ref.onDispose(c.dispose);
  return c;
});

final itemsProvider = StreamProvider<List<ItemRow>>(
  (ref) => ref.watch(dbProvider).watchItems(),
);
final completionsProvider = StreamProvider<List<CompletionRow>>(
  (ref) => ref.watch(dbProvider).watchCompletions(),
);

DateTime _todayLocal() {
  final n = DateTime.now();
  return DateTime.utc(n.year, n.month, n.day);
}

/// Today's date (as a UTC-midnight DateTime used purely as a calendar date). Refreshed on resume.
class TodayNotifier extends Notifier<DateTime> {
  @override
  DateTime build() => _todayLocal();
  void refresh() {
    final t = _todayLocal();
    if (t != state) state = t;
  }
}

final todayProvider = NotifierProvider<TodayNotifier, DateTime>(
  TodayNotifier.new,
);

class SelectedDayNotifier extends Notifier<DateTime> {
  @override
  DateTime build() => ref.watch(todayProvider);
  void select(DateTime d) => state = d;
}

final selectedDayProvider = NotifierProvider<SelectedDayNotifier, DateTime>(
  SelectedDayNotifier.new,
);

class SyncStatusNotifier extends Notifier<SyncStatus> {
  @override
  SyncStatus build() => SyncStatus.idle;
  void set(SyncStatus s) => state = s;
}

final syncStatusProvider = NotifierProvider<SyncStatusNotifier, SyncStatus>(
  SyncStatusNotifier.new,
);

List<RItem> toRItems(List<ItemRow> rows) => [
  for (final i in rows)
    RItem(
      id: i.id,
      kind: i.kind,
      weekdays: i.weekdays,
      points: i.points,
      active: i.active,
      deleted: i.deletedAt != null,
    ),
];

List<RCompletion> toRCompletions(List<CompletionRow> rows) => [
  for (final c in rows)
    RCompletion(c.itemId, c.date, deleted: c.deletedAt != null),
];

final statsProvider = Provider<Stats>((ref) {
  final items = ref.watch(itemsProvider).value ?? const <ItemRow>[];
  final comps = ref.watch(completionsProvider).value ?? const <CompletionRow>[];
  return computeStats(
    toRItems(items),
    toRCompletions(comps),
    ref.watch(todayProvider),
  );
});

const _defaultPoints = {'chore': 10, 'habit': 15, 'todo': 20};

/// Generic example chores offered from the empty state and Settings. Ids are deterministic
/// (uuid v5) so seeding on two devices cannot create duplicates. Weekdays is a bitmask,
/// bit0 = Monday .. bit6 = Sunday.
class StarterChore {
  const StarterChore(this.name, this.weekdays, this.time);
  final String name, time;
  final int weekdays;
}

const starterChores = [
  StarterChore('Take out the bins', 1, '18:00'),
  StarterChore('Water the plants', 4, '17:00'),
  StarterChore('Laundry', 16, '16:00'),
  StarterChore('Clean the bathroom', 16, '17:00'),
  StarterChore('Tidy your room', 32, '10:00'),
  StarterChore('Dishes or cook dinner', 127, '18:30'),
];

/// All user actions that change data go through here: write locally, reward, schedule a sync.
class AppController {
  AppController(this.ref);
  final Ref ref;
  Timer? _syncTimer;
  int _combo = 0;
  DateTime _lastTick = DateTime.fromMillisecondsSinceEpoch(0);

  AppDb get _db => ref.read(dbProvider);

  static int nowMs() => DateTime.now().millisecondsSinceEpoch;

  void scheduleSync() {
    _syncTimer?.cancel();
    _syncTimer = Timer(const Duration(milliseconds: 1500), syncNow);
  }

  Future<void> syncNow() async {
    SyncStatus status;
    try {
      status = await ref.read(syncEngineProvider).sync();
    } catch (_) {
      // A sync failure must never escape as an unhandled async error from a timer.
      status = SyncStatus.offline;
    }
    if (status != SyncStatus.syncing) {
      ref.read(syncStatusProvider.notifier).set(status);
    }
  }

  Future<void> toggle(ItemRow item, DateTime date, {Offset? at}) async {
    final items = ref.read(itemsProvider).value ?? const <ItemRow>[];
    final comps =
        ref.read(completionsProvider).value ?? const <CompletionRow>[];
    // A to-do is done once, on whichever day it was ticked; chores and habits are per day.
    final existingTodo = item.kind == 'todo'
        ? comps.where((c) => c.itemId == item.id).firstOrNull
        : null;
    final dayStr = existingTodo?.date ?? fmtDay(date);
    final already = comps.any((c) => c.itemId == item.id && c.date == dayStr);
    final today = ref.read(todayProvider);
    final before = computeStats(toRItems(items), toRCompletions(comps), today);

    await _db.setCompletion(item.id, dayStr, !already, nowMs());
    scheduleSync();

    final sfx = ref.read(sfxProvider);
    if (already) {
      sfx.buzz();
      return;
    }
    final after = computeStats(toRItems(items), [
      ...toRCompletions(comps),
      RCompletion(item.id, dayStr),
    ], today);
    _reward(item, dayStr, before, after, at);
  }

  void _reward(
    ItemRow item,
    String dayStr,
    Stats before,
    Stats after,
    Offset? at,
  ) {
    final fx = ref.read(fxProvider);
    final sfx = ref.read(sfxProvider);
    final p = Pal.dark; // confetti palette is accent-based; the dark set reads on both themes
    final pos = at ?? const Offset(200, 400);
    final now = DateTime.now();
    _combo = now.difference(_lastTick).inSeconds < 6 ? _combo + 1 : 1;
    _lastTick = now;

    fx.floatText('+${item.points} XP', pos - const Offset(0, 8));
    fx.burst(pos, 16 + (_combo.clamp(1, 6)) * 4, 4.2, p.confetti);
    sfx.tick(_combo - 1);
    sfx.buzz();

    final size =
        PlatformDispatcher.instance.views.first.physicalSize /
        PlatformDispatcher.instance.views.first.devicePixelRatio;
    final perfect =
        after.perfectDays.contains(dayStr) &&
        !before.perfectDays.contains(dayStr);
    final streakNow = after.streaks[item.id] ?? 0;
    final streakBefore = before.streaks[item.id] ?? 0;
    if (perfect) {
      fx.showToast('Perfect day! +$perfectDayBonus bonus XP', gold: true);
      fx.burst(Offset(size.width / 2, size.height * .3), 160, 11, p.confetti);
      fx.burst(Offset(size.width * .2, size.height * .45), 70, 8, p.confetti);
      fx.burst(Offset(size.width * .8, size.height * .45), 70, 8, p.confetti);
      sfx.fanfare();
      sfx.buzz(60);
    } else if (after.level > before.level) {
      fx.showToast('Level ${after.level}! ${after.rank}', gold: true);
      fx.burst(Offset(size.width / 2, size.height * .25), 110, 9, p.confetti);
      sfx.level();
      sfx.buzz(60);
    } else if (item.kind == 'habit' &&
        streakNow > streakBefore &&
        {3, 5, 7}.contains(streakNow)) {
      fx.showToast('$streakNow day streak, keep it burning');
      fx.burst(pos, 40, 6, p.confetti);
    } else if (_combo >= 3) {
      fx.showToast('x$_combo combo');
    }
  }

  Future<void> saveItem({
    String? id,
    required String kind,
    required String name,
    int weekdays = 127,
    String? remindTime,
    int? points,
    String? dueDate,
    bool active = true,
  }) async {
    await _db.upsertItem(
      ItemsCompanion(
        id: Value(id ?? const Uuid().v4()),
        kind: Value(kind),
        name: Value(name.trim()),
        weekdays: Value(weekdays),
        remindTime: Value(remindTime),
        points: Value(points ?? _defaultPoints[kind]!),
        dueDate: Value(dueDate),
        active: Value(active),
        updatedAt: Value(nowMs()),
        deletedAt: const Value(null),
        dirty: const Value(true),
      ),
    );
    scheduleSync();
  }

  Future<void> deleteItem(String id) async {
    await _db.softDeleteItem(id, nowMs());
    scheduleSync();
  }

  Future<int> addStarterChores() async {
    final existing = (ref.read(itemsProvider).value ?? const <ItemRow>[])
        .map((i) => i.id)
        .toSet();
    var added = 0;
    for (final c in starterChores) {
      final id = const Uuid().v5(
        Namespace.url.value,
        'bread-tracker:chore:${c.name}',
      );
      if (existing.contains(id)) continue;
      await saveItem(
        id: id,
        kind: 'chore',
        name: c.name,
        weekdays: c.weekdays,
        remindTime: c.time,
      );
      added++;
    }
    return added;
  }
}

final controllerProvider = Provider<AppController>((ref) => AppController(ref));
