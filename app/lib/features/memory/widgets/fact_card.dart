import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import '../../../core/formatting.dart';
import '../../../core/theme.dart';
import '../../../models/memory_fact.dart';
import '../../../models/memory_context.dart';
import '../../../models/memory_search_result.dart';
import '../../../widgets/id_chip.dart';
import 'episode_sheet.dart';

/// 一条记忆的卡片。
///
/// 走「编辑式档案」而不是通用卡片：左侧一条细色条是类别标记，标题 / 正文 /
/// 出处三个层级靠字号与留白分开，不靠层层嵌套的容器和投影。正文统一走
/// [plainStatement]，所以新旧两种存储格式在这里长得一样。
class FactCard extends StatelessWidget {
  const FactCard({
    super.key,
    required this.fact,
    this.channels,
    this.contextInfo,
  });
  final MemoryFact fact;
  final RetrievalChannels? channels;
  final MemoryContext? contextInfo;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final c = contextInfo;
    final canOpen = fact.sourceEpisodeId?.isNotEmpty ?? false;
    final accent = fact.invalidated
        ? scheme.error
        : CormexTheme.memoryAccent(theme.brightness);

    return Semantics(
      button: canOpen,
      child: Material(
        color: scheme.surfaceContainerLow,
        clipBehavior: Clip.antiAlias,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(CormexRadii.card),
          side: BorderSide(color: scheme.outlineVariant),
        ),
        child: InkWell(
          onTap: canOpen
              ? () => showEpisodeSheet(
                  context,
                  fact.sourceEpisodeId!,
                  memory: fact,
                  contextInfo: c,
                )
              : null,
          child: IntrinsicHeight(
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Container(width: 3, color: accent),
                Expanded(
                  child: Padding(
                    padding: const EdgeInsets.fromLTRB(
                      CormexSpacing.lg,
                      CormexSpacing.lg - 2,
                      CormexSpacing.lg,
                      CormexSpacing.md + 2,
                    ),
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Row(
                          children: [
                            Icon(
                              fact.invalidated
                                  ? Icons.history_rounded
                                  : Icons.bookmark_outline_rounded,
                              size: 16,
                              color: accent,
                            ),
                            const SizedBox(width: CormexSpacing.sm),
                            Expanded(
                              child: Text(
                                c?.subject ?? '记忆',
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: theme.textTheme.titleMedium?.copyWith(
                                  fontWeight: FontWeight.w600,
                                  letterSpacing: -0.1,
                                ),
                              ),
                            ),
                            const SizedBox(width: CormexSpacing.sm),
                            _Tag(
                              label: fact.invalidated
                                  ? '已失效'
                                  : memoryCategory(fact.predicate),
                              color: fact.invalidated ? scheme.error : accent,
                            ),
                          ],
                        ),
                        const SizedBox(height: CormexSpacing.xs + 2),
                        Text(
                          '${c?.projectName ?? '未分类项目'} · ${c?.agentLabel ?? '来源信息暂未加载'}',
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: theme.textTheme.labelSmall?.copyWith(
                            fontFamilyFallback: CormexTheme.monoFallback,
                          ),
                        ),
                        const SizedBox(height: CormexSpacing.md + 2),
                        MarkdownBody(
                          data: plainStatement(fact.statement),
                          softLineBreak: true,
                          selectable: false,
                          styleSheet:
                              MarkdownStyleSheet.fromTheme(theme).copyWith(
                            p: theme.textTheme.bodyLarge?.copyWith(
                              height: 1.65,
                              color: scheme.onSurface,
                            ),
                          ),
                          imageBuilder: (uri, title, alt) =>
                              Text('[图片${alt == null ? '' : ': $alt'}]'),
                        ),
                        const SizedBox(height: CormexSpacing.md + 2),
                        Text(
                          c == null
                              ? '自动提取 · 可查看来源'
                              : c.extractionUnit == 'qa_turn'
                              ? '完整问答 · ${c.verificationLabel}'
                              : c.verificationLabel,
                          style: theme.textTheme.labelSmall?.copyWith(
                            color: c?.verificationLabel == '助手自述'
                                ? scheme.tertiary
                                : scheme.onSurfaceVariant,
                          ),
                        ),
                        if (c?.chatTitle case final String title) ...[
                          const SizedBox(height: CormexSpacing.xs),
                          Text(
                            title.replaceAll(RegExp(r'\s+'), ' '),
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: theme.textTheme.labelSmall,
                          ),
                        ],
                        const SizedBox(height: CormexSpacing.md),
                        Divider(
                          height: 1,
                          thickness: 1,
                          color: scheme.outlineVariant,
                        ),
                        const SizedBox(height: CormexSpacing.md - 2),
                        Row(
                          children: [
                            Text(
                              formatDate(c?.sourceTime ?? fact.createdAt),
                              style: theme.textTheme.labelSmall?.copyWith(
                                fontFamilyFallback: CormexTheme.monoFallback,
                              ),
                            ),
                            const SizedBox(width: CormexSpacing.sm),
                            IdChip(id: fact.id, label: '记忆'),
                            const Spacer(),
                            if (canOpen)
                              Text(
                                '查看详情',
                                style: theme.textTheme.labelMedium?.copyWith(
                                  color: scheme.primary,
                                  fontWeight: FontWeight.w600,
                                ),
                              ),
                            if (canOpen) ...[
                              const SizedBox(width: CormexSpacing.xs),
                              Icon(
                                Icons.arrow_outward,
                                size: 14,
                                color: scheme.primary,
                              ),
                            ],
                          ],
                        ),
                      ],
                    ),
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

/// 类别标签：不用默认 Chip（它的内边距和边框在密集列表里太重）。
class _Tag extends StatelessWidget {
  const _Tag({required this.label, required this.color});
  final String label;
  final Color color;

  @override
  Widget build(BuildContext context) => Container(
    padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 3),
    decoration: BoxDecoration(
      color: color.withValues(alpha: 0.10),
      borderRadius: BorderRadius.circular(CormexRadii.chip),
    ),
    child: Text(
      label,
      style: Theme.of(context).textTheme.labelSmall?.copyWith(
        color: color,
        fontWeight: FontWeight.w600,
      ),
    ),
  );
}
