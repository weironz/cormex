import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

import '../../../core/formatting.dart';
import '../../../models/transcript_event.dart';

bool isConversationEvent(TranscriptEvent event, {String? agent}) {
  if (event.kind != TranscriptKind.userMessage &&
      event.kind != TranscriptKind.agentMessage) {
    return false;
  }
  if (event.role == 'developer' || event.role == 'system') return false;
  if (event.role == 'assistant' &&
      (event.phase == 'commentary' || event.phase == 'analysis')) {
    return false;
  }
  final text = event.plainText.trim();
  if (text.isEmpty) return false;
  const prefixes = [
    '# AGENTS.md instructions',
    '<environment_context>',
    '<recommended_plugins>',
    '<permissions instructions>',
    '<external_codex_apps_open_page>',
    '<user_instructions>',
    '<subagent_notification>',
    '<app-context>',
    '<codex_apps_open_page_instructions>',
    '<codex_apps_client_time_context>',
    '<skills_instructions>',
    '<tools>',
    'You are Codex, an agent based on ',
  ];
  if (prefixes.any(text.startsWith)) return false;
  if (agent == 'codex' &&
      event.role == null &&
      text == 'Not a git repo, skipping') {
    return false;
  }
  return true;
}

/// 一条事件的展示。
///
/// # 消息、思考、工具调用、diff 各有各的样子
///
/// 全用同一种卡片会让一段聊天变成一堵墙 —— 而工具调用和工具结果占了真实
/// 聊天的一半以上。折叠它们，把注意力留给对话本身；需要时点开看原文。
class EventTile extends StatelessWidget {
  const EventTile({super.key, required this.event, this.agent});

  final TranscriptEvent event;
  final String? agent;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);

    // `meta` 与 `unknown` 不渲染成对话：它们是聊天的一部分，但不是有人
    // 说过的话。渲染出来会让一段对话里夹杂几十条「mode」「atis-latch」。
    if (event.kind == TranscriptKind.meta ||
        event.kind == TranscriptKind.unknown) {
      return const SizedBox.shrink();
    }

    final text = event.plainText.trimLeft();
    final trimmed = text.trimRight();
    if (agent == 'codex' &&
        event.role != 'assistant' &&
        trimmed == 'Not a git repo, skipping') {
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16),
        child: _collapsible(
          context,
          icon: Icons.info_outline,
          title: '环境状态',
          body: event.plainText,
        ),
      );
    }
    const notificationStart = '<subagent_notification>';
    const notificationEnd = '</subagent_notification>';
    if (event.kind == TranscriptKind.userMessage &&
        trimmed.startsWith(notificationStart) &&
        trimmed.endsWith(notificationEnd)) {
      final raw = trimmed
          .substring(
            notificationStart.length,
            trimmed.length - notificationEnd.length,
          )
          .trim();
      String? result;
      try {
        final decoded = jsonDecode(raw);
        if (decoded is Map && decoded['status'] is Map) {
          final status = decoded['status'] as Map;
          if (status['completed'] is String) {
            result = status['completed'] as String;
          }
        }
      } on FormatException {
        // Preserve unrecognized payloads in the expandable original.
      }
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 4),
        child: Theme(
          data: theme.copyWith(dividerColor: Colors.transparent),
          child: ExpansionTile(
            leading: Icon(
              Icons.account_tree_outlined,
              size: 18,
              color: theme.colorScheme.tertiary,
            ),
            title: const Text('子任务通知'),
            subtitle: Text(result == null ? '查看原始通知' : '已完成 · 展开查看结果'),
            childrenPadding: const EdgeInsets.fromLTRB(20, 4, 20, 16),
            children: [
              if (result != null)
                MarkdownBody(
                  data: result,
                  selectable: true,
                  softLineBreak: true,
                  imageBuilder: (uri, title, alt) =>
                      Text('[图片${alt == null || alt.isEmpty ? '' : ': $alt'}]'),
                ),
              _collapsible(
                context,
                icon: Icons.code,
                title: '查看原文',
                body: trimmed,
              ),
            ],
          ),
        ),
      );
    }
    final injectedContext =
        event.role == 'developer' ||
        event.role == 'system' ||
        (event.kind == TranscriptKind.agentMessage &&
            (text.startsWith('You are Codex, an agent based on ') ||
                text.startsWith('<app-context>') ||
                text.startsWith('<codex_apps_open_page_instructions>') ||
                text.startsWith('<codex_apps_client_time_context>') ||
                text.startsWith('<skills_instructions>') ||
                text.startsWith('<tools>'))) ||
        ((event.role == 'user' || event.kind == TranscriptKind.userMessage) &&
            (text.startsWith('# AGENTS.md instructions') ||
                text.startsWith('<environment_context>') ||
                text.startsWith('<recommended_plugins>') ||
                text.startsWith('<permissions instructions>') ||
                text.startsWith('<external_codex_apps_open_page>') ||
                text.startsWith('<user_instructions>')));
    if (injectedContext) {
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16),
        child: _collapsible(
          context,
          icon: Icons.tune,
          title: '聊天上下文',
          body: event.plainText,
        ),
      );
    }

    if (event.role == 'assistant' &&
        (event.phase == 'commentary' || event.phase == 'analysis')) {
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16),
        child: _collapsible(
          context,
          icon: Icons.more_horiz,
          title: '处理过程',
          body: event.plainText,
        ),
      );
    }

    if (event.kind == TranscriptKind.agentThought &&
        event.content.length == 1 &&
        event.content.first is ThoughtBlock) {
      return Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16),
        child: _collapsible(
          context,
          icon: Icons.psychology_outlined,
          title: '思考',
          body: (event.content.first as ThoughtBlock).text,
        ),
      );
    }

    final conversation =
        event.kind == TranscriptKind.userMessage ||
        event.kind == TranscriptKind.agentMessage;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 6),
      child: Align(
        alignment: Alignment.centerLeft,
        child: Container(
          width: double.infinity,
          padding: conversation
              ? const EdgeInsets.symmetric(horizontal: 20, vertical: 18)
              : EdgeInsets.zero,
          decoration: conversation
              ? BoxDecoration(
                  color: event.kind == TranscriptKind.userMessage
                      ? theme.colorScheme.primaryContainer.withValues(
                          alpha: 0.25,
                        )
                      : theme.colorScheme.surface,
                  border: Border.all(
                    color: theme.colorScheme.outlineVariant.withValues(
                      alpha: 0.55,
                    ),
                  ),
                  borderRadius: BorderRadius.circular(16),
                )
              : null,
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              _header(theme),
              const SizedBox(height: 12),
              ...event.content.map((b) => _block(context, b)),
            ],
          ),
        ),
      ),
    );
  }

  Widget _header(ThemeData theme) {
    final label = switch (event.kind) {
      TranscriptKind.userMessage => '你',
      TranscriptKind.agentMessage => switch (agent) {
        'codex' => 'Codex',
        'claude_code' => 'Claude',
        'gemini_cli' => 'Gemini',
        _ => 'AI 助手',
      },
      TranscriptKind.agentThought => '思考',
      TranscriptKind.toolCall => '调用 ${event.toolName ?? ''}',
      TranscriptKind.toolResult => '结果',
      TranscriptKind.plan => '计划',
      TranscriptKind.system => '系统',
      _ => '',
    };
    return Row(
      children: [
        if (event.kind == TranscriptKind.userMessage ||
            event.kind == TranscriptKind.agentMessage) ...[
          CircleAvatar(
            radius: 14,
            backgroundColor: event.kind == TranscriptKind.userMessage
                ? theme.colorScheme.primaryContainer
                : theme.colorScheme.tertiaryContainer,
            child: Icon(
              event.kind == TranscriptKind.userMessage
                  ? Icons.person_outline
                  : Icons.auto_awesome_outlined,
              size: 16,
            ),
          ),
          const SizedBox(width: 10),
        ],
        Flexible(
          child: Text(
            label,
            style: theme.textTheme.labelLarge?.copyWith(
              fontWeight: FontWeight.w700,
            ),
          ),
        ),
        if (event.occurredAt != null) ...[
          const SizedBox(width: 8),
          // 绝对日期，不用「3 分钟前」：聊天可以被回放到任意时点，而相对
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
      TextBlock(:final text) => MarkdownBody(
        data: text,
        selectable: true,
        softLineBreak: true,
        styleSheet: MarkdownStyleSheet.fromTheme(theme).copyWith(
          p: theme.textTheme.bodyMedium?.copyWith(height: 1.65),
          listBullet: theme.textTheme.bodyMedium?.copyWith(height: 1.65),
        ),
        // Archives are untrusted; opening a conversation must not fetch URLs.
        imageBuilder: (uri, title, alt) =>
            Text('[图片${alt == null || alt.isEmpty ? '' : ': $alt'}]'),
      ),
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

  /// 默认折叠的一段。工具结果动辄几十 KB，展开会让一段聊天变成一堵墙。
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
