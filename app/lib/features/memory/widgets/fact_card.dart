import 'package:flutter/material.dart';

import '../../../core/formatting.dart';
import '../../../models/memory_fact.dart';
import '../../../models/memory_search_result.dart';
import 'episode_sheet.dart';

/// 一条召回的事实。
///
/// 陈述放最上面（一眼扫过去唯一值得读的就是它），然后是审计那一行：
/// 来源通道、领域、谓词、召回路、置信度，以及跳到出处的入口。
///
/// # 与 Cortex 那份的差别：`invalidated` 来自服务端，不是调用方
///
/// Cortex 里这是一个 widget 参数，只有对话抽屉那条路会传 —— 因为它那边的
/// `MemoryFact` 根本没解析线协议上的 `invalidated` 字段。于是**记忆浏览器
/// 里回放出来的失效事实与现行事实长得一模一样**，而回放正是这个字段唯一
/// 有信息量的场合。这里直接读事实自己带的那一位。
class FactCard extends StatefulWidget {
  const FactCard({super.key, required this.fact, this.channels});

  final MemoryFact fact;
  final RetrievalChannels? channels;

  @override
  State<FactCard> createState() => _FactCardState();
}

class _FactCardState extends State<FactCard> {
  bool _hovered = false;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final fact = widget.fact;
    final episodeId = fact.sourceEpisodeId;
    final canOpen = episodeId != null && episodeId.isNotEmpty;
    final hasTime = fact.validAt != null || fact.createdAt != null;

    return MouseRegion(
      onEnter: (_) => setState(() => _hovered = true),
      onExit: (_) => setState(() => _hovered = false),
      cursor: canOpen ? SystemMouseCursors.click : MouseCursor.defer,
      child: GestureDetector(
        onTap: canOpen ? () => showEpisodeSheet(context, episodeId) : null,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 120),
          margin: const EdgeInsets.only(bottom: 8),
          padding: const EdgeInsets.all(13),
          decoration: BoxDecoration(
            color: _hovered && canOpen
                ? scheme.surfaceContainerHigh
                : scheme.surfaceContainerLow,
            borderRadius: BorderRadius.circular(10),
            border: Border.all(
              // 已失效的那些整张卡片描一道错误色的边：在一屏几十张卡片里，
              // 标签太小了，扫视时看不见。而在回放视图里「哪几条已经不成立」
              // 恰恰是要一眼看出来的东西。
              color: fact.invalidated
                  ? scheme.error.withValues(alpha: 0.55)
                  : _hovered && canOpen
                  ? scheme.secondary.withValues(alpha: 0.45)
                  : scheme.outlineVariant,
            ),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(
                fact.statement,
                // **不置灰**：这是一条审计记录，读不清楚没有任何好处。
                // 「它已经不成立了」由边框与标签说，不由可读性说。
                style: theme.textTheme.bodyMedium?.copyWith(
                  color: scheme.onSurface,
                  height: 1.55,
                ),
              ),
              const SizedBox(height: 9),
              Wrap(
                spacing: 6,
                runSpacing: 6,
                crossAxisAlignment: WrapCrossAlignment.center,
                children: [
                  if (fact.invalidated)
                    _Tag(label: '已失效', color: scheme.error, filled: true),
                  // 出处排在第一个（紧跟失效徽章）：「这话是我说的，还是模型
                  // 推的」决定了上面那句陈述值多少分量，所以它该在这一行的
                  // 任何东西之前被读到。
                  if (_sourceLabel(fact.sourceChannel) case final label?)
                    _Tag(
                      label: label,
                      color: _trustColor(fact.trustTier, scheme),
                      filled: true,
                    ),
                  if (fact.domain != null)
                    _Tag(
                      label: fact.domain!,
                      color: scheme.secondary,
                      filled: true,
                    ),
                  if (fact.predicate != null)
                    _Tag(
                      label: fact.predicate!,
                      color: scheme.onSurfaceVariant,
                      mono: true,
                    ),
                  ...?widget.channels?.channels.map(
                    (c) => _Tag(
                      label: c,
                      color: _channelColor(c, scheme),
                      mono: true,
                    ),
                  ),
                  if (fact.confidence != null)
                    _Tag(
                      label: formatConfidence(fact.confidence),
                      color: _confidenceColor(fact.confidence!, scheme),
                    ),
                ],
              ),
              const SizedBox(height: 8),
              Row(
                children: [
                  if (hasTime) ...[
                    Icon(
                      Icons.schedule_rounded,
                      size: 12,
                      color: scheme.onSurfaceVariant,
                    ),
                    const SizedBox(width: 4),
                    // 两个时间都印出来，而且分别标注：这是双时间轴在界面上
                    // 唯一露脸的地方。只印一个的话，「这件事什么时候发生的」
                    // 与「你什么时候知道的」就永远混成一个。
                    Flexible(
                      child: Text(
                        '生效 ${formatDate(fact.validAt)}'
                        '${fact.createdAt != null ? ' · 记录 ${formatDate(fact.createdAt)}' : ''}',
                        style: theme.textTheme.labelSmall,
                        overflow: TextOverflow.ellipsis,
                      ),
                    ),
                  ] else
                    const Spacer(),
                  if (canOpen) ...[
                    const SizedBox(width: 8),
                    Text(
                      '出处',
                      style: theme.textTheme.labelSmall?.copyWith(
                        color: scheme.secondary,
                        fontWeight: FontWeight.w600,
                      ),
                    ),
                    Icon(
                      Icons.chevron_right_rounded,
                      size: 14,
                      color: scheme.secondary,
                    ),
                  ],
                ],
              ),
            ],
          ),
        ),
      ),
    );
  }

  /// 每一路召回一个颜色。
  ///
  /// 服务端会融合四到五路（`bm25`、`vector`、`graph`、`recency`、`episode`），
  /// 全画成一个色，这一行读起来就是一片没有差别的噪声。认不出的名字回落到
  /// 中性色而不是丢掉 —— 服务端新加的一路应该被看见，不是消失。
  static Color _channelColor(String channel, ColorScheme scheme) =>
      switch (channel) {
        'bm25' => const Color(0xFFB07219),
        'vector' => scheme.primary,
        'graph' => const Color(0xFF7A5FBF),
        'recency' => const Color(0xFF2E8B8B),
        'episode' => const Color(0xFF3F7DBF),
        _ => scheme.onSurfaceVariant,
      };

  static Color _confidenceColor(double c, ColorScheme scheme) {
    if (c >= 0.9) return const Color(0xFF2E7D32);
    if (c >= 0.75) return scheme.onSurfaceVariant;
    return const Color(0xFFB26A00);
  }

  /// 知识哪来的，用人话而不是线协议的词表。
  ///
  /// 刻意很短 —— 这个标签跟领域、谓词、召回路挤在同一行，而它是眼睛该先
  /// 落到的那一个。
  ///
  /// `unknown_legacy` 与缺失都返回 null，这是**故意把两种情况合并**：
  /// 列存在之前写下的行，以及比这个字段还老的服务端。两者都答不出任何真话，
  /// 而一个写着「未知」的标签是把噪声打扮成出处。**认不出的通道原样显示**，
  /// 不藏 —— 服务端新加一路应该看得见。
  static String? _sourceLabel(String? channel) => switch (channel) {
    'user_stated' => '你说的',
    'conversation' => '推断',
    'derived' => '派生',
    'tool_output' => '工具',
    'external' => '外部',
    null || 'unknown_legacy' => null,
    final other => other,
  };

  /// 按信任级上色，而不是按通道名。
  ///
  /// 真正给这些排序的是信任级，通道名只是它的标签。走信任级还意味着服务端
  /// 新加一路通道时，这里不用跟着改就能拿到一个说得通的颜色。
  static Color _trustColor(int? tier, ColorScheme scheme) => switch (tier) {
    1 => const Color(0xFF2E7D32), // 亲述：与高 confidence 同色，两者都是「最可信」
    2 => scheme.primary,
    3 => const Color(0xFF7A5FBF),
    _ => const Color(0xFFB26A00), // 外部及更低：与低 confidence 同色
  };
}

class _Tag extends StatelessWidget {
  const _Tag({
    required this.label,
    required this.color,
    this.filled = false,
    this.mono = false,
  });

  final String label;
  final Color color;
  final bool filled;
  final bool mono;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
      decoration: BoxDecoration(
        color: filled
            ? color.withValues(alpha: 0.12)
            : Theme.of(context).colorScheme.surfaceContainerHigh,
        borderRadius: BorderRadius.circular(5),
        border: Border.all(color: color.withValues(alpha: 0.28)),
      ),
      child: Text(
        label,
        style: TextStyle(
          fontSize: 10.5,
          height: 1.3,
          color: color,
          fontWeight: FontWeight.w600,
          fontFamily: mono ? 'monospace' : null,
        ),
      ),
    );
  }
}
