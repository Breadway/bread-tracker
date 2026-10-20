import 'dart:async';

import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import 'fx/fx.dart';
import 'reminders.dart';
import 'state.dart';
import 'theme.dart';
import 'ui/pair.dart';
import 'ui/settings.dart';
import 'ui/stats.dart';
import 'ui/today.dart';
import 'ui/todos.dart';

class TrackerApp extends ConsumerWidget {
  const TrackerApp({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    return MaterialApp(
      title: 'Bread Tracker',
      debugShowCheckedModeBanner: false,
      theme: buildTheme(Pal.light),
      darkTheme: buildTheme(Pal.dark),
      themeMode: ThemeMode.system,
      builder: (context, child) =>
          FxLayer(controller: ref.watch(fxProvider), child: child!),
      home: const Gate(),
    );
  }
}

/// Shows the pairing screen until the device is paired or the user chooses offline use.
class Gate extends ConsumerStatefulWidget {
  const Gate({super.key});
  @override
  ConsumerState<Gate> createState() => _GateState();
}

class _GateState extends ConsumerState<Gate> {
  @override
  Widget build(BuildContext context) {
    final paired = ref.watch(pairedProvider);
    final prefs = ref.watch(prefsProvider);
    final skipped = prefs.getBool('skipPair') ?? false;
    return paired.when(
      loading: () => const Scaffold(body: SizedBox.shrink()),
      error: (_, _) => const Shell(),
      data: (isPaired) {
        if (isPaired || skipped) return const Shell();
        return PairScreen(
          onSkip: () async {
            await prefs.setBool('skipPair', true);
            setState(() {});
          },
        );
      },
    );
  }
}

class Shell extends ConsumerStatefulWidget {
  const Shell({super.key});
  @override
  ConsumerState<Shell> createState() => _ShellState();
}

class _ShellState extends ConsumerState<Shell> with WidgetsBindingObserver {
  int _tab = 0;
  Timer? _poll;
  StreamSubscription<List<ConnectivityResult>>? _conn;
  StreamSubscription<ReminderPayload>? _taps;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    final ctl = ref.read(controllerProvider);
    ctl.syncNow();
    ref.read(pushProvider).register();
    _taps = ref.read(pushProvider).taps.listen(
      (p) => ref.read(openTodayProvider.notifier).request(p.date),
    );
    _poll = Timer.periodic(const Duration(seconds: 60), (_) => ctl.syncNow());
    _conn = Connectivity().onConnectivityChanged.listen((r) {
      if (r.any((c) => c != ConnectivityResult.none)) {
        ctl.syncNow();
        ref.read(pushProvider).register();
      }
    }, onError: (_) {});
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.resumed) {
      ref.read(todayProvider.notifier).refresh();
      // A notification action may have written rows from another isolate while we slept.
      ref.read(dbProvider).refreshStreams();
      ref.read(controllerProvider).syncNow();
      ref.read(pushProvider).register();
    }
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    _poll?.cancel();
    _taps?.cancel();
    _conn?.cancel().catchError((_) {});
    super.dispose();
  }

  void _openToday(OpenTodayRequest? r) {
    if (r == null) return;
    final d = r.date == null ? null : DateTime.tryParse(r.date!);
    if (d != null) {
      ref.read(selectedDayProvider.notifier).select(DateTime.utc(d.year, d.month, d.day));
    }
    setState(() => _tab = 0);
  }

  @override
  Widget build(BuildContext context) {
    ref.listen(openTodayProvider, (_, r) => _openToday(r));
    return Scaffold(
      body: IndexedStack(
        index: _tab,
        children: const [
          TodayScreen(),
          TodosScreen(),
          StatsScreen(),
          SettingsScreen(),
        ],
      ),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _tab,
        onDestinationSelected: (i) => setState(() => _tab = i),
        destinations: const [
          NavigationDestination(
            icon: Icon(Icons.calendar_today_outlined),
            selectedIcon: Icon(Icons.calendar_today),
            label: 'Today',
          ),
          NavigationDestination(
            icon: Icon(Icons.checklist_rounded),
            label: 'To-dos',
          ),
          NavigationDestination(
            icon: Icon(Icons.bar_chart_rounded),
            label: 'Stats',
          ),
          NavigationDestination(
            icon: Icon(Icons.settings_outlined),
            selectedIcon: Icon(Icons.settings),
            label: 'Settings',
          ),
        ],
      ),
    );
  }
}
