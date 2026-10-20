import 'dart:math' as math;

import 'package:flutter/material.dart';

import '../theme.dart';

class SectionHeader extends StatelessWidget {
  const SectionHeader(this.title, {super.key, this.trailing});
  final String title;
  final String? trailing;

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(0, 20, 0, 8),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        children: [
          Text(
            title.toUpperCase(),
            style: TextStyle(color: p.dim, fontSize: 12, letterSpacing: 1),
          ),
          if (trailing != null)
            Text(
              trailing!,
              style: TextStyle(
                color: p.dim,
                fontSize: 12,
                fontFeatures: const [FontFeature.tabularFigures()],
              ),
            ),
        ],
      ),
    );
  }
}

/// Bordered surface used for rows and cards.
class Panel extends StatelessWidget {
  const Panel({
    super.key,
    required this.child,
    this.padding = const EdgeInsets.all(12),
    this.highlight = false,
    this.dashed = false,
  });
  final Widget child;
  final EdgeInsets padding;
  final bool highlight;
  final bool dashed;

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return AnimatedContainer(
      duration: const Duration(milliseconds: 200),
      padding: padding,
      decoration: BoxDecoration(
        color: highlight
            ? Color.alphaBlend(p.accent.withValues(alpha: .08), p.surface)
            : p.surface,
        borderRadius: BorderRadius.circular(8),
        border: Border.all(
          color: highlight
              ? Color.alphaBlend(p.accent.withValues(alpha: .45), p.line)
              : p.line,
        ),
      ),
      child: child,
    );
  }
}

/// Checkbox row with a pop and glow when it becomes ticked.
class TickRow extends StatefulWidget {
  const TickRow({
    super.key,
    required this.title,
    required this.done,
    required this.onTap,
    this.trailing,
    this.subtitle,
    this.bare = false,
  });
  final String title;
  final String? subtitle;
  final bool done;
  final void Function(Offset globalPos) onTap;
  final Widget? trailing;
  final bool bare;

  @override
  State<TickRow> createState() => _TickRowState();
}

class _TickRowState extends State<TickRow> with SingleTickerProviderStateMixin {
  late final AnimationController _c = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 650),
  );

  @override
  void didUpdateWidget(TickRow old) {
    super.didUpdateWidget(old);
    if (!old.done && widget.done && !MediaQuery.disableAnimationsOf(context)) {
      _c.forward(from: 0);
    }
  }

  @override
  void dispose() {
    _c.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final row = GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTapUp: (d) => widget.onTap(d.globalPosition),
      child: Semantics(
        checked: widget.done,
        label: widget.title,
        child: Row(
          children: [
            AnimatedBuilder(
              animation: _c,
              builder: (_, _) {
                final t = _c.value;
                final scale = widget.done && _c.isAnimating
                    ? 1 + .35 * math.sin(math.pi * math.min(1, t * 2.2))
                    : 1.0;
                return Transform.scale(
                  scale: scale,
                  child: Container(
                    width: 24,
                    height: 24,
                    decoration: BoxDecoration(
                      color: widget.done ? p.accent : Colors.transparent,
                      border: Border.all(
                        color: widget.done ? p.accent : p.dim,
                        width: 2,
                      ),
                      borderRadius: BorderRadius.circular(6),
                      boxShadow: _c.isAnimating
                          ? [
                              BoxShadow(
                                color: p.accent.withValues(alpha: .6 * (1 - t)),
                                blurRadius: 4,
                                spreadRadius: 14 * t,
                              ),
                            ]
                          : null,
                    ),
                    child: widget.done
                        ? CustomPaint(
                            painter: _CheckPainter(
                              p.onAccent,
                              _c.isAnimating ? math.min(1, t * 3) : 1,
                            ),
                          )
                        : null,
                  ),
                );
              },
            ),
            const SizedBox(width: 12),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  AnimatedDefaultTextStyle(
                    duration: const Duration(milliseconds: 200),
                    style: TextStyle(
                      fontFamily: kFont,
                      fontSize: 14,
                      color: widget.done ? p.dim : p.fg,
                      decoration: widget.done
                          ? TextDecoration.lineThrough
                          : TextDecoration.none,
                      decorationColor: p.dim,
                    ),
                    child: Text(widget.title),
                  ),
                  if (widget.subtitle != null)
                    Text(
                      widget.subtitle!,
                      style: TextStyle(color: p.dim, fontSize: 12),
                    ),
                ],
              ),
            ),
            if (widget.trailing != null) widget.trailing!,
          ],
        ),
      ),
    );
    if (widget.bare) return row;
    return Panel(highlight: widget.done, child: row);
  }
}

class _CheckPainter extends CustomPainter {
  _CheckPainter(this.color, this.t);
  final Color color;
  final double t;

  @override
  void paint(Canvas canvas, Size size) {
    final path = Path()
      ..moveTo(size.width * .24, size.height * .52)
      ..lineTo(size.width * .43, size.height * .70)
      ..lineTo(size.width * .78, size.height * .32);
    final m = path.computeMetrics().first;
    canvas.drawPath(
      m.extractPath(0, m.length * t),
      Paint()
        ..color = color
        ..style = PaintingStyle.stroke
        ..strokeWidth = 3
        ..strokeCap = StrokeCap.round
        ..strokeJoin = StrokeJoin.round,
    );
  }

  @override
  bool shouldRepaint(_CheckPainter old) => old.t != t || old.color != color;
}

class ProgressRing extends StatelessWidget {
  const ProgressRing({super.key, required this.done, required this.total});
  final int done, total;

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    final full = total > 0 && done == total;
    final frac = total == 0 ? 0.0 : done / total;
    return SizedBox(
      width: 72,
      height: 72,
      child: TweenAnimationBuilder<double>(
        tween: Tween(begin: 0, end: frac),
        duration: const Duration(milliseconds: 350),
        curve: Curves.easeOut,
        builder: (_, v, _) => CustomPaint(
          painter: _RingPainter(
            v,
            p.raised,
            full ? p.gold : p.accent,
            full ? p.gold : null,
          ),
          child: Center(
            child: Text(
              '$done/$total',
              style: TextStyle(
                fontWeight: FontWeight.w700,
                fontSize: 16,
                fontFeatures: const [FontFeature.tabularFigures()],
                color: p.fg,
              ),
            ),
          ),
        ),
      ),
    );
  }
}

class _RingPainter extends CustomPainter {
  _RingPainter(this.v, this.track, this.bar, this.glow);
  final double v;
  final Color track, bar;
  final Color? glow;

  @override
  void paint(Canvas canvas, Size size) {
    final rect = Rect.fromCircle(
      center: size.center(Offset.zero),
      radius: size.width / 2 - 5,
    );
    final base = Paint()
      ..style = PaintingStyle.stroke
      ..strokeWidth = 8
      ..strokeCap = StrokeCap.round;
    canvas.drawArc(rect, 0, math.pi * 2, false, base..color = track);
    if (glow != null) {
      canvas.drawArc(
        rect,
        -math.pi / 2,
        math.pi * 2 * v,
        false,
        Paint()
          ..style = PaintingStyle.stroke
          ..strokeWidth = 12
          ..strokeCap = StrokeCap.round
          ..color = glow!.withValues(alpha: .35)
          ..maskFilter = const MaskFilter.blur(BlurStyle.normal, 6),
      );
    }
    canvas.drawArc(
      rect,
      -math.pi / 2,
      math.pi * 2 * v,
      false,
      base..color = bar,
    );
  }

  @override
  bool shouldRepaint(_RingPainter old) => old.v != v || old.bar != bar;
}

class XpBar extends StatelessWidget {
  const XpBar({
    super.key,
    required this.level,
    required this.rank,
    required this.into,
    required this.per,
    this.trailing,
  });
  final int level, into, per;
  final String rank;
  final Widget? trailing;

  @override
  Widget build(BuildContext context) {
    final p = palOf(context);
    return Panel(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      child: Row(
        children: [
          SizedBox(
            width: 44,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(
                  '$level',
                  style: TextStyle(
                    color: p.gold,
                    fontSize: 20,
                    fontWeight: FontWeight.w700,
                    height: 1,
                    fontFeatures: const [FontFeature.tabularFigures()],
                  ),
                ),
                Text('level', style: TextStyle(color: p.dim, fontSize: 12)),
              ],
            ),
          ),
          const SizedBox(width: 12),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: [
                    Text(
                      rank,
                      style: TextStyle(
                        color: p.fg,
                        fontSize: 12,
                        fontWeight: FontWeight.w700,
                      ),
                    ),
                    Text(
                      '$into / $per XP',
                      style: TextStyle(
                        color: p.dim,
                        fontSize: 12,
                        fontFeatures: const [FontFeature.tabularFigures()],
                      ),
                    ),
                  ],
                ),
                const SizedBox(height: 4),
                ClipRRect(
                  borderRadius: BorderRadius.circular(999),
                  child: SizedBox(
                    height: 10,
                    child: Stack(
                      children: [
                        Container(color: p.raised),
                        TweenAnimationBuilder<double>(
                          tween: Tween(begin: 0, end: (into / per).clamp(0, 1)),
                          duration: const Duration(milliseconds: 500),
                          curve: Curves.easeOutBack,
                          builder: (_, v, _) => FractionallySizedBox(
                            widthFactor: v.clamp(0.0, 1.0),
                            heightFactor: 1,
                            alignment: Alignment.centerLeft,
                            child: DecoratedBox(
                              decoration: BoxDecoration(
                                gradient: LinearGradient(
                                  colors: [p.accent, p.gold],
                                ),
                              ),
                            ),
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              ],
            ),
          ),
          if (trailing != null) ...[const SizedBox(width: 12), trailing!],
        ],
      ),
    );
  }
}

String fmtTime(String hhmm) {
  final parts = hhmm.split(':');
  var h = int.parse(parts[0]);
  final m = parts[1];
  final ap = h >= 12 ? 'PM' : 'AM';
  h = h % 12 == 0 ? 12 : h % 12;
  return '$h:$m $ap';
}

const dayShort = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
const dayFull = [
  'Monday',
  'Tuesday',
  'Wednesday',
  'Thursday',
  'Friday',
  'Saturday',
  'Sunday',
];
const monthShort = [
  'Jan',
  'Feb',
  'Mar',
  'Apr',
  'May',
  'Jun',
  'Jul',
  'Aug',
  'Sep',
  'Oct',
  'Nov',
  'Dec',
];
