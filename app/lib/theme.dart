import 'package:flutter/material.dart';

/// Design tokens from the approved mockup, on the bread dark base.
class Pal {
  const Pal({
    required this.bg,
    required this.surface,
    required this.raised,
    required this.line,
    required this.fg,
    required this.dim,
    required this.accent,
    required this.onAccent,
    required this.good,
    required this.gold,
    required this.pink,
    required this.brightness,
  });

  final Color bg, surface, raised, line, fg, dim, accent, onAccent, good, gold, pink;
  final Brightness brightness;

  static const dark = Pal(
    bg: Color(0xFF0C0C0C),
    surface: Color(0xFF1A1A1A),
    raised: Color(0xFF242424),
    line: Color(0xFF2E2C2A),
    fg: Color(0xFFE8E8E8),
    dim: Color(0xFF9A948C),
    accent: Color(0xFFD9A05B),
    onAccent: Color(0xFF1A1209),
    good: Color(0xFF8FBF7F),
    gold: Color(0xFFF2C14E),
    pink: Color(0xFFE58AA8),
    brightness: Brightness.dark,
  );

  static const light = Pal(
    bg: Color(0xFFF6F3EE),
    surface: Color(0xFFFFFFFF),
    raised: Color(0xFFEFEAE2),
    line: Color(0xFFDDD5C9),
    fg: Color(0xFF1D1A16),
    dim: Color(0xFF6F675C),
    accent: Color(0xFFB4691F),
    onAccent: Color(0xFFFFF8EE),
    good: Color(0xFF3F7A34),
    gold: Color(0xFFC28A0E),
    pink: Color(0xFFB8446C),
    brightness: Brightness.light,
  );

  List<Color> get confetti => [accent, gold, good, const Color(0xFF6F8FB5), pink, const Color(0xFFF4EAD8)];
}

Pal palOf(BuildContext context) => Theme.of(context).brightness == Brightness.dark ? Pal.dark : Pal.light;

const kFont = 'VarelaRound';

ThemeData buildTheme(Pal p) {
  final base = ThemeData(brightness: p.brightness, useMaterial3: true, fontFamily: kFont);
  return base.copyWith(
    scaffoldBackgroundColor: p.bg,
    colorScheme: ColorScheme.fromSeed(seedColor: p.accent, brightness: p.brightness).copyWith(
      primary: p.accent,
      onPrimary: p.onAccent,
      surface: p.surface,
      onSurface: p.fg,
    ),
    textTheme: base.textTheme.apply(bodyColor: p.fg, displayColor: p.fg, fontFamily: kFont),
    dividerColor: p.line,
    inputDecorationTheme: InputDecorationTheme(
      filled: true,
      fillColor: p.surface,
      contentPadding: const EdgeInsets.symmetric(horizontal: 16, vertical: 12),
      border: OutlineInputBorder(borderRadius: BorderRadius.circular(6), borderSide: BorderSide(color: p.line)),
      enabledBorder: OutlineInputBorder(borderRadius: BorderRadius.circular(6), borderSide: BorderSide(color: p.line)),
      focusedBorder: OutlineInputBorder(borderRadius: BorderRadius.circular(6), borderSide: BorderSide(color: p.accent, width: 2)),
    ),
    filledButtonTheme: FilledButtonThemeData(
      style: FilledButton.styleFrom(
        backgroundColor: p.accent,
        foregroundColor: p.onAccent,
        textStyle: const TextStyle(fontFamily: kFont, fontWeight: FontWeight.w700),
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 12),
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
      ),
    ),
    navigationBarTheme: NavigationBarThemeData(
      backgroundColor: p.surface,
      indicatorColor: p.raised,
      labelTextStyle: WidgetStatePropertyAll(TextStyle(fontFamily: kFont, fontSize: 12, color: p.dim)),
      iconTheme: WidgetStateProperty.resolveWith(
        (s) => IconThemeData(color: s.contains(WidgetState.selected) ? p.accent : p.dim),
      ),
    ),
  );
}
