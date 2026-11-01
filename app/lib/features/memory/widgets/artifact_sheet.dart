import 'package:flutter/material.dart';

import '../../../core/formatting.dart';
import '../../../models/memory_artifact.dart';
import '../../../models/memory_context.dart';
import '../../sessions/session_detail_page.dart';
import 'memory_reading.dart';

Future<void> showArtifactSheet(BuildContext context, MemoryArtifact artifact) =>
    showMemoryReadingDialog(
      context,
      child: _ArtifactDetail(artifact: artifact),
    );

class _ArtifactDetail extends StatelessWidget {
  const _ArtifactDetail({required this.artifact});
  final MemoryArtifact artifact;

  @override
  Widget build(BuildContext context) {
    final a = artifact;
    final theme = Theme.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(24, 16, 12, 16),
          child: Row(
            children: [
              Icon(Icons.article_outlined, color: theme.colorScheme.primary),
              const SizedBox(width: 10),
              Expanded(
                child: Text(
                  '${a.kind.label}详情',
                  style: theme.textTheme.titleLarge,
                ),
              ),
              IconButton(
                tooltip: '关闭',
                onPressed: () => Navigator.of(context).pop(),
                icon: const Icon(Icons.close),
              ),
            ],
          ),
        ),
        const Divider(height: 1),
        Expanded(
          child: SingleChildScrollView(
            padding: const EdgeInsets.all(24),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                SelectableText(
                  a.displayTitle,
                  style: theme.textTheme.headlineSmall,
                ),
                const SizedBox(height: 12),
                Wrap(
                  spacing: 16,
                  runSpacing: 8,
                  children: [
                    Text('项目：${a.projectName ?? '未分类'}'),
                    Text(formatAbsolute(a.createdAt)),
                  ],
                ),
                const SizedBox(height: 12),
                Text(
                  a.kind.description,
                  style: theme.textTheme.bodyMedium?.copyWith(
                    color: theme.colorScheme.onSurfaceVariant,
                  ),
                ),
                const SizedBox(height: 20),
                MemoryTextPanel(
                  child: a.markdown.trim().isEmpty
                      ? const Text('这条记录没有正文。')
                      : MemoryMarkdown(a.markdown),
                ),
                if (a.reason case final String reason
                    when reason.trim().isNotEmpty) ...[
                  const SizedBox(height: 24),
                  Text('摘要说明', style: theme.textTheme.titleMedium),
                  const SizedBox(height: 8),
                  MemoryMarkdown(reason),
                ],
                const Divider(height: 36),
                Text('来源证据', style: theme.textTheme.titleLarge),
                const SizedBox(height: 12),
                if (a.evidence.isEmpty) const Text('摘要依据完整问答生成，可打开来源聊天查看全部内容。'),
                for (final evidence in a.evidence) ...[
                  Text(
                    memorySourceRoleLabel(evidence.role),
                    style: theme.textTheme.labelLarge,
                  ),
                  const SizedBox(height: 8),
                  MemoryTextPanel(child: SelectableText(evidence.quote)),
                  const SizedBox(height: 16),
                ],
                if (a.sessionId.isNotEmpty)
                  ListTile(
                    contentPadding: EdgeInsets.zero,
                    leading: const Icon(Icons.chat_bubble_outline),
                    title: const Text('查看来源聊天'),
                    subtitle: Text(
                      a.chatTitle ?? '查看完整问答与上下文',
                      maxLines: 2,
                      overflow: TextOverflow.ellipsis,
                    ),
                    trailing: const Icon(Icons.open_in_new, size: 18),
                    onTap: () {
                      final nav = Navigator.of(context);
                      nav.pop();
                      nav.push(
                        MaterialPageRoute<void>(
                          builder: (_) =>
                              SessionDetailPage(sessionId: a.sessionId),
                        ),
                      );
                    },
                  ),
                if (a.sourceEventIds.isNotEmpty || a.evidence.isNotEmpty)
                  ExpansionTile(
                    tilePadding: EdgeInsets.zero,
                    title: const Text('来源消息标识'),
                    children: [
                      Align(
                        alignment: Alignment.centerLeft,
                        child: SelectableText(
                          {
                            ...a.sourceEventIds,
                            for (final e in a.evidence) e.messageId,
                          }.join('\n'),
                        ),
                      ),
                    ],
                  ),
              ],
            ),
          ),
        ),
      ],
    );
  }
}
