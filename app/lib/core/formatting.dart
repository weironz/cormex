import 'package:intl/intl.dart';

final _full = DateFormat('yyyy年M月d日 HH:mm');
final _dateOnly = DateFormat('yyyy-MM-dd');
final _monthDay = DateFormat('M月d日');

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

/// 列表行尾那种窄位置上的日期：今年的省掉年份，其余同 [formatDate]。
///
/// 仍然是绝对日期（理由见 [formatDate]），只是更短。
String formatCompactDate(DateTime? when, {DateTime? now}) {
  if (when == null) return '—';
  final year = (now ?? DateTime.now()).year;
  return when.year == year ? _monthDay.format(when) : _dateOnly.format(when);
}

String formatConfidence(double? c) => c == null ? '' : '${(c * 100).round()}%';

/// 记忆的正文：一句纯文本结论，不管库里存的是新格式还是老格式。
///
/// 老版本让提取器写 Markdown「审计报告」（`**来源报告**`、`## 主题`、空行
/// 分段），列表行把它压平后就剩一串 `**`，卡片又塞进 108px 的滚动框。新版
/// 提取器写一句话，但库里已有的记忆不会自动变，所以展示层统一净化。
/// 反引号保留：行内代码在纯文本里仍读得懂，去掉反而让标识符与正文粘连。
String plainStatement(String statement) {
  final joined = statement
      .split('\n')
      .map((raw) {
        var line = raw.trim().replaceAll('**', '');
        line = line.replaceFirst(RegExp(r'^#{1,6}\s*'), '');
        line = line.replaceFirst(RegExp(r'^[-*+]\s+'), '');
        return line.trim();
      })
      .where((line) => line.isNotEmpty)
      .join(' ');
  return joined
      .replaceAll(RegExp(r'\s+'), ' ')
      .replaceAll('来源报告', '助手自述，未独立核实')
      .trim();
}

/// 把 ULID 缩短成 `01KZC66ZT7…R6C`。
String shortId(String? id, {int head = 8, int tail = 4}) {
  if (id == null || id.isEmpty) return '—';
  if (id.length <= head + tail + 1) return id;
  return '${id.substring(0, head)}…${id.substring(id.length - tail)}';
}
