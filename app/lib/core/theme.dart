import 'package:flutter/material.dart';

/// Cormex 的设计令牌。
///
/// 手写而不是 `ColorScheme.fromSeed`：生成出来的色调表会给每一层表面都带上
/// 看得见的色偏，观感是「一个 Flutter demo」。这里的表面接近中性（偏冷的灰），
/// 单一的靛蓝重音只留给真正可交互的东西。
///
/// 记忆相关的东西另有一支青色（[memoryAccentLight] / [memoryAccentDark]）：
/// 出处、信任级、时间回放这些是这个产品的主角，让它们跟「主按钮」抢同一种
/// 颜色，等于让主角和布景穿一样的衣服。
/// 间距节奏：4 的倍数，但刻意不匀速。列表内紧、区块之间松，靠差值而不是
/// 靠加边框来分层 —— 均匀间距是「一个 Flutter demo」最明显的特征。
abstract final class CormexSpacing {
  static const double xs = 4;
  static const double sm = 8;
  static const double md = 12;
  static const double lg = 20;
  static const double xl = 32;
  static const double gutter = 24;
}

/// 圆角：卡片比控件大，控件比标签大。全线统一，避免每个页面各写各的 10/12/14。
abstract final class CormexRadii {
  static const double control = 10;
  static const double card = 16;
  static const double sheet = 20;
  static const double chip = 8;
}

abstract final class CormexTheme {
  static const _indigo = Color(0xFF5B62F4);
  static const _indigoLight = Color(0xFF7A80FF);

  static const memoryAccentLight = Color(0xFF0E7C86);
  static const memoryAccentDark = Color(0xFF4FD1C5);

  /// 当前亮度下的记忆青。
  static Color memoryAccent(Brightness b) =>
      b == Brightness.dark ? memoryAccentDark : memoryAccentLight;

  static ThemeData light() => _build(_lightScheme, Brightness.light);
  static ThemeData dark() => _build(_darkScheme, Brightness.dark);

  static const _lightScheme = ColorScheme(
    brightness: Brightness.light,
    primary: _indigo,
    onPrimary: Colors.white,
    primaryContainer: Color(0xFFE4E5FF),
    onPrimaryContainer: Color(0xFF1B1D66),
    secondary: memoryAccentLight,
    onSecondary: Colors.white,
    secondaryContainer: Color(0xFFD3F1F3),
    onSecondaryContainer: Color(0xFF04353A),
    error: Color(0xFFB3261E),
    onError: Colors.white,
    errorContainer: Color(0xFFF9DEDC),
    onErrorContainer: Color(0xFF410E0B),
    surface: Color(0xFFFCFCFD),
    onSurface: Color(0xFF1A1C1F),
    onSurfaceVariant: Color(0xFF5A5F66),
    outline: Color(0xFFC3C6CF),
    outlineVariant: Color(0xFFE3E5EA),
    surfaceContainerLowest: Colors.white,
    surfaceContainerLow: Color(0xFFF7F8FA),
    surfaceContainer: Color(0xFFF2F3F6),
    surfaceContainerHigh: Color(0xFFECEEF2),
    surfaceContainerHighest: Color(0xFFE6E8EE),
    inverseSurface: Color(0xFF2E3033),
    onInverseSurface: Color(0xFFF1F1F3),
  );

  static const _darkScheme = ColorScheme(
    brightness: Brightness.dark,
    primary: _indigoLight,
    onPrimary: Color(0xFF10123D),
    primaryContainer: Color(0xFF32357F),
    onPrimaryContainer: Color(0xFFE0E1FF),
    secondary: memoryAccentDark,
    onSecondary: Color(0xFF00312F),
    secondaryContainer: Color(0xFF0C4A4C),
    onSecondaryContainer: Color(0xFFB8F0EC),
    error: Color(0xFFF2B8B5),
    onError: Color(0xFF601410),
    errorContainer: Color(0xFF8C1D18),
    onErrorContainer: Color(0xFFF9DEDC),
    surface: Color(0xFF101114),
    onSurface: Color(0xFFE4E5E9),
    onSurfaceVariant: Color(0xFF9CA1AC),
    outline: Color(0xFF3A3D45),
    outlineVariant: Color(0xFF2A2D34),
    surfaceContainerLowest: Color(0xFF0B0C0E),
    surfaceContainerLow: Color(0xFF15171A),
    surfaceContainer: Color(0xFF1A1C20),
    surfaceContainerHigh: Color(0xFF212429),
    surfaceContainerHighest: Color(0xFF292C32),
    inverseSurface: Color(0xFFE4E5E9),
    onInverseSurface: Color(0xFF1A1C20),
  );

  static ThemeData _build(ColorScheme scheme, Brightness brightness) {
    final base = ThemeData(
      useMaterial3: true,
      colorScheme: scheme,
      brightness: brightness,
    );

    return base.copyWith(
      scaffoldBackgroundColor: scheme.surface,
      // 页头与内容同色、不投影：分隔交给一条细线。Material 3 默认在滚动时
      // 给页头染一层主色调，配上左右分栏，看起来像三块颜色不同的面板。
      appBarTheme: AppBarTheme(
        backgroundColor: scheme.surface,
        foregroundColor: scheme.onSurface,
        surfaceTintColor: Colors.transparent,
        scrolledUnderElevation: 0,
        elevation: 0,
        centerTitle: false,
        titleSpacing: 20,
        toolbarHeight: 60,
        titleTextStyle: base.textTheme.titleLarge?.copyWith(
          fontSize: 17,
          fontWeight: FontWeight.w600,
          letterSpacing: -0.2,
          color: scheme.onSurface,
          fontFamilyFallback: _cjkFallback,
        ),
        shape: Border(bottom: BorderSide(color: scheme.outlineVariant)),
      ),
      navigationRailTheme: NavigationRailThemeData(
        indicatorColor: scheme.primaryContainer,
        selectedIconTheme: IconThemeData(color: scheme.onPrimaryContainer),
        unselectedIconTheme: IconThemeData(color: scheme.onSurfaceVariant),
        selectedLabelTextStyle: TextStyle(
          fontSize: 12,
          fontWeight: FontWeight.w600,
          color: scheme.onSurface,
        ),
        unselectedLabelTextStyle: TextStyle(
          fontSize: 12,
          color: scheme.onSurfaceVariant,
        ),
        minWidth: 76,
      ),
      navigationBarTheme: NavigationBarThemeData(
        backgroundColor: scheme.surfaceContainerLow,
        indicatorColor: scheme.primaryContainer,
        surfaceTintColor: Colors.transparent,
        height: 64,
      ),
      dividerTheme: DividerThemeData(
        color: scheme.outlineVariant,
        thickness: 1,
        space: 1,
      ),
      textTheme: _textTheme(base.textTheme, scheme),
      inputDecorationTheme: InputDecorationTheme(
        filled: true,
        fillColor: scheme.surfaceContainerHigh,
        hintStyle: TextStyle(color: scheme.onSurfaceVariant),
        contentPadding: const EdgeInsets.symmetric(
          horizontal: 14,
          vertical: 12,
        ),
        border: _inputBorder(scheme.outlineVariant),
        enabledBorder: _inputBorder(Colors.transparent),
        focusedBorder: _inputBorder(scheme.primary, width: 1.5),
        isDense: true,
      ),
      filledButtonTheme: FilledButtonThemeData(
        style: FilledButton.styleFrom(
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(10),
          ),
          padding: const EdgeInsets.symmetric(horizontal: 18, vertical: 14),
          textStyle: const TextStyle(fontWeight: FontWeight.w600),
        ),
      ),
      iconButtonTheme: IconButtonThemeData(
        style: IconButton.styleFrom(
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(9)),
        ),
      ),
      tooltipTheme: TooltipThemeData(
        waitDuration: const Duration(milliseconds: 500),
        decoration: BoxDecoration(
          color: scheme.inverseSurface,
          borderRadius: BorderRadius.circular(6),
        ),
        textStyle: TextStyle(color: scheme.onInverseSurface, fontSize: 12),
      ),
      chipTheme: ChipThemeData(
        side: BorderSide(color: scheme.outlineVariant),
        backgroundColor: scheme.surfaceContainer,
        labelStyle: TextStyle(fontSize: 12, color: scheme.onSurfaceVariant),
        padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 0),
      ),
      scrollbarTheme: ScrollbarThemeData(
        thickness: const WidgetStatePropertyAll(8),
        radius: const Radius.circular(4),
        thumbColor: WidgetStatePropertyAll(
          scheme.onSurfaceVariant.withValues(alpha: 0.35),
        ),
      ),
      // 组件层：让「没被单独设计过」的页面也继承同一套形状与留白，而不是
      // 掉回 Material 默认值。卡片不再靠投影分层，用一道细边框 + 表面色差。
      cardTheme: CardThemeData(
        elevation: 0,
        surfaceTintColor: Colors.transparent,
        color: scheme.surfaceContainerLow,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(CormexRadii.card),
          side: BorderSide(color: scheme.outlineVariant),
        ),
        margin: const EdgeInsets.symmetric(
          horizontal: CormexSpacing.md,
          vertical: CormexSpacing.sm,
        ),
      ),
      listTileTheme: ListTileThemeData(
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(CormexRadii.control),
        ),
        contentPadding: const EdgeInsets.symmetric(
          horizontal: CormexSpacing.md,
          vertical: CormexSpacing.xs,
        ),
        minVerticalPadding: CormexSpacing.sm,
      ),
      outlinedButtonTheme: OutlinedButtonThemeData(
        style: OutlinedButton.styleFrom(
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(CormexRadii.control),
          ),
          side: BorderSide(color: scheme.outlineVariant),
          padding: const EdgeInsets.symmetric(
            horizontal: CormexSpacing.lg - 2,
            vertical: CormexSpacing.md,
          ),
          textStyle: const TextStyle(fontWeight: FontWeight.w600),
        ),
      ),
      textButtonTheme: TextButtonThemeData(
        style: TextButton.styleFrom(
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(CormexRadii.control),
          ),
          padding: const EdgeInsets.symmetric(
            horizontal: CormexSpacing.md,
            vertical: CormexSpacing.sm,
          ),
        ),
      ),
      dialogTheme: DialogThemeData(
        elevation: 0,
        surfaceTintColor: Colors.transparent,
        backgroundColor: scheme.surface,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(CormexRadii.sheet),
          side: BorderSide(color: scheme.outlineVariant),
        ),
      ),
      bottomSheetTheme: BottomSheetThemeData(
        elevation: 0,
        surfaceTintColor: Colors.transparent,
        backgroundColor: scheme.surface,
        shape: const RoundedRectangleBorder(
          borderRadius: BorderRadius.vertical(
            top: Radius.circular(CormexRadii.sheet),
          ),
        ),
      ),
      tabBarTheme: TabBarThemeData(
        indicatorColor: scheme.primary,
        labelColor: scheme.onSurface,
        unselectedLabelColor: scheme.onSurfaceVariant,
        dividerColor: scheme.outlineVariant,
        labelStyle: const TextStyle(fontWeight: FontWeight.w600),
      ),
      snackBarTheme: SnackBarThemeData(
        elevation: 0,
        behavior: SnackBarBehavior.floating,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(CormexRadii.control),
        ),
      ),
      progressIndicatorTheme: ProgressIndicatorThemeData(
        color: scheme.primary,
        linearTrackColor: scheme.surfaceContainerHighest,
      ),
      splashFactory: InkSparkle.splashFactory,
    );
  }

  static OutlineInputBorder _inputBorder(Color color, {double width = 1}) =>
      OutlineInputBorder(
        borderRadius: BorderRadius.circular(10),
        borderSide: BorderSide(color: color, width: width),
      );

  static TextTheme _textTheme(TextTheme base, ColorScheme scheme) => base
      .copyWith(
        titleLarge: base.titleLarge?.copyWith(
          fontSize: 17,
          fontWeight: FontWeight.w600,
          letterSpacing: -0.2,
        ),
        titleMedium: base.titleMedium?.copyWith(
          fontSize: 14.5,
          fontWeight: FontWeight.w600,
        ),
        titleSmall: base.titleSmall?.copyWith(
          fontSize: 12,
          fontWeight: FontWeight.w600,
          letterSpacing: 0.4,
        ),
        bodyLarge: base.bodyLarge?.copyWith(fontSize: 15, height: 1.62),
        bodyMedium: base.bodyMedium?.copyWith(fontSize: 14, height: 1.6),
        bodySmall: base.bodySmall?.copyWith(
          fontSize: 12.5,
          height: 1.5,
          color: scheme.onSurfaceVariant,
        ),
        labelSmall: base.labelSmall?.copyWith(
          fontSize: 11,
          letterSpacing: 0.3,
          color: scheme.onSurfaceVariant,
        ),
      )
      .apply(fontFamilyFallback: _cjkFallback);

  /// Flutter 默认的 `Roboto` 没有中日韩字形；不显式给回退链，中文在
  /// Windows 的某些区域设置下会渲染成豆腐块，在 Web 上则每台机器挑到的
  /// 字面都不一样。
  static const _cjkFallback = <String>[
    'Microsoft YaHei UI',
    'Microsoft YaHei',
    'PingFang SC',
    'Hiragino Sans GB',
    'Noto Sans CJK SC',
    'Source Han Sans SC',
    'sans-serif',
  ];

  /// 等宽栈，给 id 与谓词用。
  static const monoFallback = <String>[
    'JetBrains Mono',
    'Cascadia Code',
    'Consolas',
    'SF Mono',
    'Menlo',
    'DejaVu Sans Mono',
    'monospace',
  ];
}
