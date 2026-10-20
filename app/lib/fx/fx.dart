import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';

import '../theme.dart';

class _Particle {
  _Particle(this.pos, this.vel, this.size, this.color, this.shape)
    : rot = math.Random().nextDouble() * 6,
      spin = (math.Random().nextDouble() - .5) * .4;
  Offset pos, vel;
  final double size;
  final Color color;
  final int shape;
  double rot, spin, life = 1;
}

class _Float {
  _Float(this.text, this.pos);
  final String text;
  final Offset pos;
  double age = 0;
}

/// Particles, floating "+XP" text and the toast pill. One controller, painted by [FxLayer].
class FxController extends ChangeNotifier {
  final _rng = math.Random();
  final particles = <_Particle>[];
  final floats = <_Float>[];
  String? toast;
  bool toastGold = false;
  double _toastLeft = 0;
  bool reduceMotion = false;

  bool get active =>
      particles.isNotEmpty || floats.isNotEmpty || _toastLeft > 0;

  void burst(Offset origin, int count, double power, List<Color> colors) {
    if (reduceMotion) return;
    for (var i = 0; i < count; i++) {
      final a = _rng.nextDouble() * math.pi * 2;
      final s = (_rng.nextDouble() * .6 + .4) * power;
      particles.add(
        _Particle(
          origin,
          Offset(math.cos(a) * s, math.sin(a) * s - power * .5),
          _rng.nextDouble() * 4 + 3,
          colors[i % colors.length],
          i % 3,
        ),
      );
    }
    if (particles.length > 400) {
      particles.removeRange(0, particles.length - 400);
    }
    notifyListeners();
  }

  void floatText(String text, Offset pos) {
    if (reduceMotion) return;
    floats.add(_Float(text, pos));
    notifyListeners();
  }

  void showToast(String message, {bool gold = false}) {
    toast = message;
    toastGold = gold;
    _toastLeft = 1.9;
    notifyListeners();
  }

  /// Advance by [dt] seconds, where 1.0 is one 60 fps frame.
  void step(double dt, double secs, Size area) {
    for (final p in particles) {
      p.vel = Offset(
        p.vel.dx * math.pow(.985, dt).toDouble(),
        p.vel.dy + .28 * dt,
      );
      p.pos += p.vel * dt;
      p.rot += p.spin * dt;
      p.life -= .009 * dt;
    }
    particles.removeWhere((p) => p.life <= 0 || p.pos.dy > area.height + 20);
    for (final f in floats) {
      f.age += secs;
    }
    floats.removeWhere((f) => f.age > .95);
    if (_toastLeft > 0) {
      _toastLeft -= secs;
      if (_toastLeft <= 0) toast = null;
    }
    notifyListeners();
  }
}

class FxScope extends InheritedNotifier<FxController> {
  const FxScope({
    super.key,
    required FxController controller,
    required super.child,
  }) : super(notifier: controller);
  static FxController of(BuildContext context) =>
      context.getInheritedWidgetOfExactType<FxScope>()!.notifier!;
}

class FxLayer extends StatefulWidget {
  const FxLayer({super.key, required this.controller, required this.child});
  final FxController controller;
  final Widget child;

  @override
  State<FxLayer> createState() => _FxLayerState();
}

class _FxLayerState extends State<FxLayer> with SingleTickerProviderStateMixin {
  late final Ticker _ticker;
  Duration _last = Duration.zero;

  @override
  void initState() {
    super.initState();
    _ticker = createTicker(_onTick);
    widget.controller.addListener(_maybeStart);
  }

  void _maybeStart() {
    if (widget.controller.active && !_ticker.isActive) {
      _last = Duration.zero;
      _ticker.start();
    }
  }

  void _onTick(Duration elapsed) {
    final secs = _last == Duration.zero
        ? 1 / 60
        : (elapsed - _last).inMicroseconds / 1e6;
    _last = elapsed;
    final size = MediaQuery.sizeOf(context);
    widget.controller.step((secs * 60).clamp(0, 3), secs.clamp(0, .1), size);
    if (!widget.controller.active) _ticker.stop();
  }

  @override
  void dispose() {
    widget.controller.removeListener(_maybeStart);
    _ticker.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = widget.controller;
    c.reduceMotion = MediaQuery.disableAnimationsOf(context);
    final p = palOf(context);
    return FxScope(
      controller: c,
      child: Stack(
        children: [
          widget.child,
          Positioned.fill(
            child: IgnorePointer(
              child: ListenableBuilder(
                listenable: c,
                builder: (_, _) => RepaintBoundary(
                  child: CustomPaint(painter: _FxPainter(c, p)),
                ),
              ),
            ),
          ),
          Positioned(
            top: MediaQuery.paddingOf(context).top + 12,
            left: 16,
            right: 16,
            child: IgnorePointer(
              child: ListenableBuilder(
                listenable: c,
                builder: (_, _) => AnimatedSlide(
                  duration: const Duration(milliseconds: 280),
                  curve: Curves.easeOutBack,
                  offset: c.toast == null ? const Offset(0, -3) : Offset.zero,
                  child: Center(
                    child: DecoratedBox(
                      decoration: BoxDecoration(
                        color: c.toastGold ? p.gold : p.accent,
                        borderRadius: BorderRadius.circular(999),
                        boxShadow: const [
                          BoxShadow(
                            color: Color(0x59000000),
                            blurRadius: 24,
                            offset: Offset(0, 8),
                          ),
                        ],
                      ),
                      child: Padding(
                        padding: const EdgeInsets.symmetric(
                          horizontal: 20,
                          vertical: 8,
                        ),
                        child: Text(
                          c.toast ?? '',
                          textAlign: TextAlign.center,
                          style: TextStyle(
                            fontFamily: kFont,
                            color: p.onAccent,
                            fontWeight: FontWeight.w700,
                            fontSize: 14,
                            decoration: TextDecoration.none,
                          ),
                        ),
                      ),
                    ),
                  ),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _FxPainter extends CustomPainter {
  _FxPainter(this.c, this.p) : super(repaint: c);
  final FxController c;
  final Pal p;

  @override
  void paint(Canvas canvas, Size size) {
    final paint = Paint();
    for (final q in c.particles) {
      paint.color = q.color.withValues(alpha: q.life.clamp(0, 1));
      canvas.save();
      canvas.translate(q.pos.dx, q.pos.dy);
      canvas.rotate(q.rot);
      switch (q.shape) {
        case 0:
          canvas.drawRect(
            Rect.fromCenter(
              center: Offset.zero,
              width: q.size * 2,
              height: q.size,
            ),
            paint,
          );
        case 1:
          canvas.drawCircle(Offset.zero, q.size / 1.5, paint);
        default:
          canvas.drawRect(
            Rect.fromCenter(
              center: Offset.zero,
              width: q.size,
              height: q.size * 2,
            ),
            paint,
          );
      }
      canvas.restore();
    }
    for (final f in c.floats) {
      final t = (f.age / .9).clamp(0.0, 1.0);
      final scale = t < .3 ? .8 + (t / .3) * .45 : 1.25 - (t - .3) / .7 * .25;
      final tp = TextPainter(
        text: TextSpan(
          text: f.text,
          style: TextStyle(
            fontFamily: kFont,
            fontWeight: FontWeight.w700,
            fontSize: 16 * scale,
            color: p.gold.withValues(alpha: (1 - t).clamp(0, 1)),
            shadows: const [
              Shadow(
                color: Color(0x73000000),
                blurRadius: 6,
                offset: Offset(0, 1),
              ),
            ],
          ),
        ),
        textDirection: TextDirection.ltr,
      )..layout();
      tp.paint(
        canvas,
        f.pos + Offset(-tp.width / 2, -56 * Curves.easeOut.transform(t)),
      );
    }
  }

  @override
  bool shouldRepaint(_FxPainter old) => true;
}
