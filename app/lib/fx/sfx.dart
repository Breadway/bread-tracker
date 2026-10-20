import 'package:audioplayers/audioplayers.dart';
import 'package:flutter/services.dart';

/// Reward sounds and haptics. Everything is best effort: a missing audio backend
/// (tests, emulators without audio) must never break a tick.
abstract class Sfx {
  bool sound = true;
  bool haptics = true;
  Future<void> init();
  void tick(int step);
  void fanfare();
  void level();
  void buzz([int ms = 15]);
}

class NoSfx extends Sfx {
  @override
  Future<void> init() async {}
  @override
  void tick(int step) {}
  @override
  void fanfare() {}
  @override
  void level() {}
  @override
  void buzz([int ms = 15]) {}
}

class AudioSfx extends Sfx {
  final _pools = <String, AudioPool>{};

  @override
  Future<void> init() async {
    try {
      for (var i = 0; i < 10; i++) {
        _pools['tick$i'] = await AudioPool.createFromAsset(
          path: 'sfx/tick$i.wav',
          maxPlayers: 3,
        );
      }
      _pools['fanfare'] = await AudioPool.createFromAsset(
        path: 'sfx/fanfare.wav',
        maxPlayers: 1,
      );
      _pools['level'] = await AudioPool.createFromAsset(
        path: 'sfx/level.wav',
        maxPlayers: 1,
      );
    } catch (_) {}
  }

  void _play(String name) {
    if (!sound) return;
    try {
      _pools[name]?.start(volume: 0.8);
    } catch (_) {}
  }

  @override
  void tick(int step) => _play('tick${step.clamp(0, 9)}');
  @override
  void fanfare() => _play('fanfare');
  @override
  void level() => _play('level');

  @override
  void buzz([int ms = 15]) {
    if (!haptics) return;
    try {
      ms > 30 ? HapticFeedback.heavyImpact() : HapticFeedback.lightImpact();
    } catch (_) {}
  }
}
