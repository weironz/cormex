import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import '../../../core/formatting.dart';
import '../../../models/memory_fact.dart';
import '../../../models/memory_context.dart';
import '../../../models/memory_search_result.dart';
import '../../../widgets/id_chip.dart';
import 'episode_sheet.dart';

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
    return Semantics(
      button: canOpen,
      child: Material(
        color: scheme.surfaceContainerLow,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(14),
          side: BorderSide(
            color: fact.invalidated ? scheme.error : scheme.outlineVariant,
          ),
        ),
        child: InkWell(
          borderRadius: BorderRadius.circular(14),
          onTap: canOpen
              ? () => showEpisodeSheet(
                  context,
                  fact.sourceEpisodeId!,
                  memory: fact,
                  contextInfo: c,
                )
              : null,
          child: Padding(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                Row(
                  children: [
                    Icon(
                      Icons.bookmark_outline_rounded,
                      size: 18,
                      color: scheme.primary,
                    ),
                    const SizedBox(width: 8),
                    Expanded(
                      child: Text(
                        c?.subject ?? '记忆',
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: theme.textTheme.titleSmall,
                      ),
                    ),
                    const SizedBox(width: 8),
                    Text(
                      fact.invalidated ? '已失效' : memoryCategory(fact.predicate),
                      style: theme.textTheme.labelSmall?.copyWith(
                        color: fact.invalidated ? scheme.error : scheme.primary,
                      ),
                    ),
                  ],
                ),
                const SizedBox(height: 5),
                Text(
                  '${c?.projectName ?? '未分类项目'} · ${c?.agentLabel ?? '来源信息暂未加载'}',
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: theme.textTheme.labelSmall,
                ),
                const SizedBox(height: 12),
                ConstrainedBox(
                  constraints: const BoxConstraints(maxHeight: 108),
                  child: SingleChildScrollView(
                    child: MarkdownBody(
                      data: fact.statement,
                      softLineBreak: true,
                      selectable: false,
                      styleSheet: MarkdownStyleSheet.fromTheme(theme).copyWith(
                        p: theme.textTheme.bodyMedium?.copyWith(height: 1.6),
                      ),
                      imageBuilder: (uri, title, alt) =>
                          Text('[图片${alt == null ? '' : ': $alt'}]'),
                    ),
                  ),
                ),
                const SizedBox(height: 12),
                Text(
                  c == null
                      ? '自动提取 · 可查看来源'
                      : c.extractionUnit == 'qa_turn'
                      ? '完整问答 · ${c.verificationLabel}'
                      : c.verificationLabel,
                  style: theme.textTheme.labelSmall?.copyWith(
                    color: c?.verificationLabel == '来源报告'
                        ? scheme.tertiary
                        : scheme.onSurfaceVariant,
                  ),
                ),
                if (c?.chatTitle case final String title) ...[
                  const SizedBox(height: 4),
                  Text(
                    title.replaceAll(RegExp(r'\s+'), ' '),
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.labelSmall,
                  ),
                ],
                const SizedBox(height: 10),
                Row(
                  children: [
                    Text(
                      formatDate(c?.sourceTime ?? fact.createdAt),
                      style: theme.textTheme.labelSmall,
                    ),
                    const SizedBox(width: 6),
                    IdChip(id: fact.id, label: '记忆'),
                    const Spacer(),
                    if (canOpen)
                      Text(
                        '查看详情',
                        style: theme.textTheme.labelMedium?.copyWith(
                          color: scheme.primary,
                        ),
                      ),
                    if (canOpen)
                      Icon(
                        Icons.arrow_outward,
                        size: 14,
                        color: scheme.primary,
                      ),
                  ],
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
