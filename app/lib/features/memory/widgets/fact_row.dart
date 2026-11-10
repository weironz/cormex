import 'package:flutter/material.dart';

import '../../../core/formatting.dart';
import '../../../core/theme.dart';
import '../../../models/memory_context.dart';
import '../../../models/memory_fact.dart';
import '../../../widgets/id_chip.dart';
import 'episode_sheet.dart';

/// 列表视图里的一行：一句陈述，加上「是什么、哪来的、什么时候、哪条」。
///
/// 卡片给一条记忆整块地方读；这一行是给「扫一遍有哪些」用的，所以陈述只留
/// 两行，其余压成一行灰字。点开看到的详情与卡片完全一样。
class FactRow extends StatelessWidget {
  const FactRow({super.key, required this.fact, this.contextInfo});
  final MemoryFact fact;
  final MemoryContext? contextInfo;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final c = contextInfo;
    final episode = fact.sourceEpisodeId;
    final accent = fact.invalidated
        ? scheme.error
        : CormexTheme.memoryAccent(theme.brightness);
    final meta = [
      fact.invalidated ? '已失效' : memoryCategory(fact.predicate),
      ?c?.subject,
      c?.projectName ?? '未分类项目',
      formatDate(c?.sourceTime ?? fact.createdAt),
    ];
    return InkWell(
      onTap: episode == null || episode.isEmpty
          ? null
          : () => showEpisodeSheet(
              context,
              episode,
              memory: fact,
              contextInfo: c,
            ),
      child: Padding(
        padding: const EdgeInsets.symmetric(
          horizontal: CormexSpacing.sm,
          vertical: CormexSpacing.md,
        ),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Container(
              width: 28,
              height: 28,
              decoration: BoxDecoration(
                color: accent.withValues(alpha: 0.10),
                borderRadius: BorderRadius.circular(CormexRadii.chip),
              ),
              child: Icon(
                fact.invalidated
                    ? Icons.history_rounded
                    : Icons.bookmark_outline_rounded,
                size: 15,
                color: accent,
              ),
            ),
            const SizedBox(width: CormexSpacing.md),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisSize: MainAxisSize.min,
                children: [
                  Text(
                    plainStatement(fact.statement),
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.bodyMedium?.copyWith(
                      height: 1.55,
                      decoration: fact.invalidated
                          ? TextDecoration.lineThrough
                          : null,
                    ),
                  ),
                  const SizedBox(height: CormexSpacing.sm - 2),
                  Wrap(
                    crossAxisAlignment: WrapCrossAlignment.center,
                    spacing: CormexSpacing.sm,
                    children: [
                      Text(
                        meta.join(' · '),
                        style: theme.textTheme.labelSmall?.copyWith(
                          color: fact.invalidated
                              ? scheme.error
                              : scheme.onSurfaceVariant,
                          fontFamilyFallback: CormexTheme.monoFallback,
                        ),
                      ),
                      IdChip(id: fact.id, label: '记忆'),
                    ],
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
