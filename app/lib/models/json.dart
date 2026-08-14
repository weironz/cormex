/// 宽容的 JSON 取值助手。
///
/// 客户端故意容忍缺字段与松类型，而不是抛异常：这个界面要能对着**比它新**
/// 和**比它旧**的 cortexd 都活下来。真正必需的东西在调用点自己校验。
library;

String asString(Object? v, [String fallback = '']) =>
    v is String ? v : (v == null ? fallback : v.toString());

String? asStringOrNull(Object? v) {
  if (v == null) return null;
  final s = v is String ? v : v.toString();
  return s.isEmpty ? null : s;
}

double? asDoubleOrNull(Object? v) => switch (v) {
  final double d => d,
  final int i => i.toDouble(),
  final String s => double.tryParse(s),
  _ => null,
};

/// 与 [asInt] 的区别：把「没有这个字段」和「有，值是 0」分开。
///
/// 凡是 0 本身有意义的地方都要用它 —— `expires_in_secs: 0` 说的是
/// 「马上就要被拒了」，跟一个兜底默认值想表达的意思正好相反。
int? asIntOrNull(Object? v) => switch (v) {
  final int i => i,
  final double d => d.toInt(),
  final String s => int.tryParse(s),
  _ => null,
};

int asInt(Object? v, [int fallback = 0]) => switch (v) {
  final int i => i,
  final double d => d.toInt(),
  final String s => int.tryParse(s) ?? fallback,
  _ => fallback,
};

bool asBool(Object? v, [bool fallback = false]) => switch (v) {
  final bool b => b,
  'true' => true,
  'false' => false,
  _ => fallback,
};

DateTime? asDateOrNull(Object? v) {
  if (v == null) return null;
  if (v is DateTime) return v;
  if (v is int) return DateTime.fromMillisecondsSinceEpoch(v, isUtc: true);
  if (v is String && v.isNotEmpty) return DateTime.tryParse(v)?.toLocal();
  return null;
}

List<String> asStringList(Object? v) {
  if (v is! List) return const [];
  return v.map(asString).where((e) => e.isNotEmpty).toList(growable: false);
}

List<Map<String, dynamic>> asObjectList(Object? v) {
  if (v is! List) return const [];
  return v.whereType<Map<String, dynamic>>().toList(growable: false);
}
