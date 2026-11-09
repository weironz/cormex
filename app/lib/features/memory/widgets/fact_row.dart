import 'package:flutter/material.dart';

import '../../../core/formatting.dart';
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
        padding: const EdgeInsets.symmetric(horizontal: 4, vertical: 10),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Padding(
              padding: const EdgeInsets.only(top: 2),
              child: Icon(
                fact.invalidated
                    ? Icons.history_rounded
                    : Icons.bookmark_outline_rounded,
                size: 16,
                color: fact.invalidated ? scheme.error : scheme.primary,
              ),
            ),
            const SizedBox(width: 10),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    fact.statement.replaceAll(RegExp(r'\s+'), ' '),
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    style: theme.textTheme.bodyMedium?.copyWith(
                      decoration: fact.invalidated
                          ? TextDecoration.lineThrough
                          : null,
                    ),
                  ),
                  const SizedBox(height: 3),
                  Wrap(
                    crossAxisAlignment: WrapCrossAlignment.center,
                    spacing: 8,
                    children: [
                      Text(
                        meta.join(' · '),
                        style: theme.textTheme.labelSmall?.copyWith(
                          color: fact.invalidated ? scheme.error : null,
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
