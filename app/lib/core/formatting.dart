import 'package:intl/intl.dart';

final _full = DateFormat('yyyy年M月d日 HH:mm');
final _dateOnly = DateFormat('yyyy-MM-dd');

/// 出处那一类时间戳：精确比简短重要。
String formatAbsolute(DateTime? when) =>
    when == null ? '时间未知' : _full.format(when);

/// 事实卡片与时间轴上的日期。
///
/// 一律绝对日期，不用「3 天前」：记忆是可以被回放到任意时点的，而相对时间
/// 的基准点是「现在」—— 回放到三个月前那一刻，满屏的「2 天前」说的是相对
/// 于今天还是相对于那一刻，没有人答得上来。
String formatDate(DateTime? when) =>
    when == null ? '—' : _dateOnly.format(when);

String formatConfidence(double? c) => c == null ? '' : '${(c * 100).round()}%';

/// 把 ULID 缩短成 `01KZC66ZT7…R6C`。
String shortId(String? id, {int head = 8, int tail = 4}) {
  if (id == null || id.isEmpty) return '—';
  if (id.length <= head + tail + 1) return id;
  return '${id.substring(0, head)}…${id.substring(id.length - tail)}';
}
