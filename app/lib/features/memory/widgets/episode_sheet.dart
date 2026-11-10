import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../../../core/formatting.dart';
import '../../../models/archive_session.dart';
import '../../../models/episode.dart';
import '../../../models/memory_fact.dart';
import '../../../models/memory_context.dart';
import '../../../state/connection_controller.dart';
import '../../../state/memory_controller.dart';
import '../../../state/memory_context.dart';
import '../../../widgets/empty_state.dart';
import '../../sessions/session_detail_page.dart';
import '../../../widgets/id_chip.dart';
import 'memory_reading.dart';

final _sourceChatProvider = FutureProvider.family<ArchiveSession, String>(
  (ref, id) async => (await ref.watch(apiProvider).archiveSession(id)).session,
);

Future<void> showEpisodeSheet(
  BuildContext context,
  String episodeId, {
  MemoryFact? memory,
  MemoryContext? contextInfo,
}) {
  return showMemoryReadingDialog(
    context,
    child: _MemoryDetail(
      episodeId: episodeId,
      memory: memory,
      contextInfo: contextInfo,
    ),
  );
}

class _MemoryDetail extends ConsumerWidget {
  const _MemoryDetail({required this.episodeId, this.memory, this.contextInfo});
  final String episodeId;
  final MemoryFact? memory;
  final MemoryContext? contextInfo;
  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final theme = Theme.of(context);
    final f = memory;
    final c =
        contextInfo ??
        (f == null
            ? null
            : ref.watch(memoryContextsProvider(f.id)).asData?.value[f.id]);
    final source = ref.watch(episodeProvider(episodeId));
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(24, 16, 12, 16),
          child: Row(
            children: [
              Icon(
                Icons.bookmark_outline_rounded,
                color: theme.colorScheme.primary,
              ),
              const SizedBox(width: 10),
              Expanded(
                child: Text(
                  f == null ? '记忆出处' : '记忆详情',
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
                if (f != null) ...[
                  Text(
                    c?.subject ?? '记忆',
                    style: theme.textTheme.headlineSmall,
                  ),
                  // 正文先读。属性是审计用的，统一降为灰色小字放在后面，
                  // 而不是让类型 / 项目 / 证据 / 两个 ID 抢在内容前面。
                  const SizedBox(height: 12),
                  MemoryTextPanel(child: MemoryMarkdown(plainStatement(f.statement))),
                  const SizedBox(height: 20),
                  DefaultTextStyle(
                    style: theme.textTheme.labelSmall!.copyWith(
                      color: theme.colorScheme.onSurfaceVariant,
                    ),
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Wrap(
                          spacing: 12,
                          runSpacing: 6,
                          children: [
                            Text('类型：${memoryCategory(f.predicate)}'),
                            Text('项目：${c?.projectName ?? '未分类'}'),
                            if (c != null)
                              Text('来源：${c.agentLabel} · ${c.roleLabel}'),
                            if (c != null) Text('证据：${c.verificationLabel}'),
                          ],
                        ),
                        const SizedBox(height: 6),
                        IdLine(label: '记忆 ID', id: f.id),
                        if (c != null) IdLine(label: '会话 ID', id: c.sessionId),
                      ],
                    ),
                  ),
                  const SizedBox(height: 16),
                  if (c?.reason case final String reason
                      when reason.trim().isNotEmpty) ...[
                    Text('自动保留依据', style: theme.textTheme.titleSmall),
                    const SizedBox(height: 8),
                    MemoryMarkdown(reason),
                  ] else ...[
                    Text('用途', style: theme.textTheme.titleSmall),
                    const SizedBox(height: 6),
                    Text(memoryPurpose(f.predicate)),
                  ],
                  const SizedBox(height: 16),
                  if (c?.extractionUnit == 'single_message')
                    Text(
                      '提取粒度：单条${c!.roleLabel}。旧提取没有同时携带配对问答和完整聊天背景。',
                      style: TextStyle(color: theme.colorScheme.tertiary),
                    ),
                  if (c?.extractionUnit == 'qa_turn')
                    const Text('提取粒度：完整问答（用户提问与 AI 答复），结合有限上下文判断。'),
                  if (c?.verificationDescription case final String description)
                    Padding(
                      padding: const EdgeInsets.only(top: 8),
                      child: Text(description),
                    ),
                  ExpansionTile(
                    tilePadding: EdgeInsets.zero,
                    title: const Text('提取属性'),
                    children: [
                      Align(
                        alignment: Alignment.centerLeft,
                        child: Text(
                          '关系类型：${memoryCategory(f.predicate)} · 领域：${f.domain ?? '未记录'}\n模型自评忠实度：${f.confidence == null ? '未记录' : '${(f.confidence! * 100).round()}%'}',
                        ),
                      ),
                    ],
                  ),
                  const Divider(height: 32),
                ],
                Text('来源证据', style: theme.textTheme.titleLarge),
                const SizedBox(height: 12),
                source.when(
                  loading: () => const LinearProgressIndicator(),
                  error: (e, _) => EmptyState(
                    icon: Icons.link_off_outlined,
                    title: '来源暂时不可读',
                    description: '$e',
                  ),
                  data: (ep) => _Evidence(
                    episode: ep,
                    qaTurn: c?.extractionUnit == 'qa_turn',
                  ),
                ),
              ],
            ),
          ),
        ),
      ],
    );
  }
}

class _Evidence extends ConsumerStatefulWidget {
  const _Evidence({required this.episode, this.qaTurn = false});
  final Episode episode;
  final bool qaTurn;
  @override
  ConsumerState<_Evidence> createState() => _EvidenceState();
}

class _EvidenceState extends ConsumerState<_Evidence> {
  bool _raw = false;
  @override
  Widget build(BuildContext context) {
    final ep = widget.episode;
    final theme = Theme.of(context);
    final qa = widget.qaTurn ? _QaSource.tryParse(ep.text) : null;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Wrap(
          spacing: 18,
          runSpacing: 8,
          children: [
            Text(widget.qaTurn ? '完整问答' : memorySourceRoleLabel(ep.role)),
            Text(formatAbsolute(ep.occurredAt)),
          ],
        ),
        const SizedBox(height: 12),
        ref
            .watch(_sourceChatProvider(ep.sessionId))
            .when(
              loading: () => const Text('正在读取来源聊天…'),
              error: (_, _) => const Text('此输入记录没有可打开的聊天档案。'),
              data: (chat) => ListTile(
                contentPadding: EdgeInsets.zero,
                leading: const Icon(Icons.chat_bubble_outline),
                title: const Text('查看来源聊天'),
                subtitle: Text(
                  chat.displayTitle,
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
                          SessionDetailPage(sessionId: ep.sessionId),
                    ),
                  );
                },
              ),
            ),
        const SizedBox(height: 12),
        SegmentedButton<bool>(
          segments: const [
            ButtonSegment(
              value: false,
              label: Text('阅读视图'),
              icon: Icon(Icons.article_outlined),
            ),
            ButtonSegment(
              value: true,
              label: Text('原文'),
              icon: Icon(Icons.code),
            ),
          ],
          selected: {_raw},
          onSelectionChanged: (v) => setState(() => _raw = v.first),
        ),
        const SizedBox(height: 14),
        if (ep.text == null)
          const MemoryTextPanel(child: Text('这条输入没有纯文本内容。'))
        else if (_raw)
          MemoryTextPanel(
            child: SelectableText(
              ep.text!,
              style: theme.textTheme.bodyMedium?.copyWith(height: 1.7),
            ),
          )
        else if (qa != null)
          _QaReading(source: qa)
        else
          MemoryTextPanel(child: MemoryMarkdown(ep.text!)),
        const SizedBox(height: 12),
        Text(
          qa == null
              ? '阅读视图渲染 Markdown；原文保留存储的文字和换行。完整背景可从来源聊天查看。'
              : '阅读视图按真实角色展示本轮问答，有限背景单独折叠。原文完整保留来源 JSON 与元数据。',
          style: theme.textTheme.bodySmall,
        ),
      ],
    );
  }
}

class _QaSource {
  const _QaSource(this.messages, this.recentContext, this.previousSummary);
  final List<_QaMessage> messages, recentContext;
  final String? previousSummary;

  static _QaSource? tryParse(String? text) {
    if (text == null) return null;
    try {
      final value = jsonDecode(text);
      if (value is! Map<String, dynamic>) return null;
      final messages = _messages(value['messages']);
      final context = _messages(value['recent_context'] ?? const []);
      final summary = value['previous_summary'];
      if (messages == null ||
          context == null ||
          (summary != null && summary is! String)) {
        return null;
      }
      return _QaSource(messages, context, summary as String?);
    } on FormatException {
      return null;
    }
  }

  static List<_QaMessage>? _messages(Object? value) {
    if (value is! List) return null;
    final messages = <_QaMessage>[];
    for (final item in value) {
      if (item is! Map<String, dynamic> ||
          item['role'] is! String ||
          item['text'] is! String) {
        return null;
      }
      messages.add(
        _QaMessage(
          role: item['role'] as String,
          text: item['text'] as String,
          occurredAt: item['occurred_at'] is String
              ? DateTime.tryParse(item['occurred_at'] as String)?.toLocal()
              : null,
        ),
      );
    }
    return messages;
  }
}

class _QaMessage {
  const _QaMessage({required this.role, required this.text, this.occurredAt});
  final String role, text;
  final DateTime? occurredAt;
}

class _QaReading extends StatelessWidget {
  const _QaReading({required this.source});
  final _QaSource source;

  @override
  Widget build(BuildContext context) {
    final summary = source.previousSummary;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text('本轮完整问答', style: Theme.of(context).textTheme.titleMedium),
        const SizedBox(height: 12),
        if (source.messages.isEmpty) const Text('本轮没有可显示的对话消息。'),
        for (final message in source.messages)
          _QaMessageBlock(message: message),
        if (source.recentContext.isNotEmpty ||
            summary?.trim().isNotEmpty == true)
          ExpansionTile(
            tilePadding: EdgeInsets.zero,
            title: const Text('有限背景'),
            subtitle: const Text('前序问答与此前摘要，不属于本轮问答'),
            children: [
              for (final message in source.recentContext)
                _QaMessageBlock(message: message),
              if (summary != null && summary.trim().isNotEmpty) ...[
                Align(
                  alignment: Alignment.centerLeft,
                  child: Text(
                    '此前会话摘要',
                    style: Theme.of(context).textTheme.titleSmall,
                  ),
                ),
                const SizedBox(height: 8),
                MemoryTextPanel(child: MemoryMarkdown(summary)),
                const SizedBox(height: 12),
              ],
            ],
          ),
      ],
    );
  }
}

class _QaMessageBlock extends StatelessWidget {
  const _QaMessageBlock({required this.message});
  final _QaMessage message;

  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.only(bottom: 16),
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Wrap(
          spacing: 16,
          runSpacing: 6,
          children: [
            Text(
              memorySourceRoleLabel(message.role),
              style: Theme.of(context).textTheme.labelLarge,
            ),
            if (message.occurredAt != null)
              Text(formatAbsolute(message.occurredAt)),
          ],
        ),
        const SizedBox(height: 8),
        MemoryTextPanel(
          child: message.text.isEmpty
              ? const Text('这条消息没有文本内容。')
              : MemoryMarkdown(message.text),
        ),
      ],
    ),
  );
}
