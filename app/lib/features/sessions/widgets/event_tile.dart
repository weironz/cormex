import 'package:flutter/material.dart';

import '../../../core/formatting.dart';
import '../../../models/transcript_event.dart';

/// 一条事件的展示。
///
/// # 消息、思考、工具调用、diff 各有各的样子
///
/// 全用同一种卡片会让一段会话变成一堵墙 —— 而工具调用和工具结果占了真实
/// 会话的一半以上。折叠它们，把注意力留给对话本身；需要时点开看原文。
class EventTile extends StatelessWidget {
  const EventTile({super.key, required this.event});

  final TranscriptEvent event;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);

    // `meta` 与 `unknown` 不渲染成对话：它们是会话的一部分，但不是有人
    // 说过的话。渲染出来会让一段对话里夹杂几十条「mode」「atis-latch」。
    if (event.kind == TranscriptKind.meta ||
        event.kind == TranscriptKind.unknown) {
      return const SizedBox.shrink();
    }

    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 6),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _header(theme),
          const SizedBox(height: 4),
          ...event.content.map((b) => _block(context, b)),
        ],
      ),
    );
  }

  Widget _header(ThemeData theme) {
    final label = switch (event.kind) {
      TranscriptKind.userMessage => '用户',
      TranscriptKind.agentMessage => 'agent',
      TranscriptKind.agentThought => '思考',
      TranscriptKind.toolCall => '调用 ${event.toolName ?? ''}',
      TranscriptKind.toolResult => '结果',
      TranscriptKind.plan => '计划',
      TranscriptKind.system => '系统',
      _ => '',
    };
    return Row(
      children: [
        Text(label, style: theme.textTheme.labelMedium),
        if (event.occurredAt != null) ...[
          const SizedBox(width: 8),
          // 绝对日期，不用「3 分钟前」：会话可以被回放到任意时点，而相对
          // 时间的基准点是「现在」—— 回放时没人答得上来那是相对于哪一刻
          Text(
            formatAbsolute(event.occurredAt),
            style: theme.textTheme.labelSmall?.copyWith(
              color: theme.colorScheme.outline,
            ),
          ),
        ],
      ],
    );
  }

  Widget _block(BuildContext context, ContentBlock block) {
    final theme = Theme.of(context);
    return switch (block) {
      TextBlock(:final text) => SelectableText(text),
      ThoughtBlock(:final text) => _collapsible(
        context,
        icon: Icons.psychology_outlined,
        title: '思考',
        body: text,
      ),
      ToolCallBlock() => _collapsible(
        context,
        icon: Icons.build_outlined,
        title: '${block.name}: ${block.summary}',
        body: block.input?.toString() ?? '',
      ),
      ToolResultBlock(:final output, :final isError) => _collapsible(
        context,
        icon: isError ? Icons.error_outline : Icons.article_outlined,
        title: isError ? '工具出错' : '工具结果',
        body: output,
        color: isError ? theme.colorScheme.error : null,
      ),
      DiffBlock(:final path, :final oldContent, :final newContent) =>
        _collapsible(
          context,
          icon: Icons.difference_outlined,
          title: '改动 $path',
          body: '--- 旧\n$oldContent\n+++ 新\n$newContent',
        ),
      PlanBlock(:final entries) => Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          for (final e in entries)
            Padding(
              padding: const EdgeInsets.only(bottom: 2),
              child: Text('• $e'),
            ),
        ],
      ),
      ImageBlock(:final blobRef) => Text(
        '[图片 ${shortId(blobRef)}]',
        style: theme.textTheme.bodySmall,
      ),
      ResourceRefBlock(:final length) => Text(
        '[大块内容 $length 字节，在原文里查看]',
        style: theme.textTheme.bodySmall,
      ),
      UnknownBlock(:final rawType) => Text(
        '[$rawType]',
        style: theme.textTheme.bodySmall?.copyWith(
          color: theme.colorScheme.outline,
        ),
      ),
    };
  }

  /// 默认折叠的一段。工具结果动辄几十 KB，展开会让一段会话变成一堵墙。
  Widget _collapsible(
    BuildContext context, {
    required IconData icon,
    required String title,
    required String body,
    Color? color,
  }) {
    final theme = Theme.of(context);
    return Theme(
      data: theme.copyWith(dividerColor: Colors.transparent),
      child: ExpansionTile(
        tilePadding: EdgeInsets.zero,
        childrenPadding: const EdgeInsets.only(left: 32, bottom: 8),
        leading: Icon(icon, size: 18, color: color),
        title: Text(
          title,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: theme.textTheme.bodyMedium?.copyWith(color: color),
        ),
        children: [
          Align(
            alignment: Alignment.centerLeft,
            child: SelectableText(
              body,
              style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
            ),
          ),
        ],
      ),
    );
  }
}
