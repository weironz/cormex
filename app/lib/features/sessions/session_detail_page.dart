import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../api/api_exception.dart';
import '../../core/agents.dart';
import '../../core/formatting.dart';
import '../../models/archive_session.dart';
import '../../models/transcript_event.dart';
import '../../state/connection_controller.dart';
import '../../state/reading_preferences.dart';
import '../../state/sessions_controller.dart';
import '../../widgets/empty_state.dart';
import 'widgets/event_tile.dart';
import 'widgets/archive_source_label.dart';
import 'widgets/session_memory_panel.dart';

/// 一段聊天的对话视图。
///
/// # 虚拟列表 + 滚到底翻页
///
/// 一段聊天可以有一千多条事件，全部一次性渲染会让滚动卡死。这里用
/// `ListView.builder`（只构建可见的那几条），并在接近底部时按游标取下一页。
///
/// # 解析失败要说出来
///
/// `projection_status == failed` 时显示原因，而不是一段空对话 —— 空对话
/// 读起来像「这段聊天没内容」，而实际是「解析器在这份输入上失败了」。
class SessionDetailPage extends ConsumerStatefulWidget {
  const SessionDetailPage({
    super.key,
    required this.sessionId,
    this.embedded = false,
  });

  final String sessionId;

  /// 嵌在聊天档案右栏里，而不是单独推入的一页：没有返回按钮可按。
  final bool embedded;

  @override
  ConsumerState<SessionDetailPage> createState() => _SessionDetailPageState();
}

class _SessionDetailPageState extends ConsumerState<SessionDetailPage> {
  final ScrollController _scroll = ScrollController();

  ArchiveSessionDetail? _detail;
  final List<TranscriptEvent> _events = [];
  String? _cursor;
  String? _error;
  bool _loading = true;
  bool _loadingMore = false;
  bool _digest = false;
  bool _showProcess = false;

  @override
  void initState() {
    super.initState();
    _scroll.addListener(_maybeLoadMore);
    _load();
  }

  @override
  void dispose() {
    _scroll.dispose();
    super.dispose();
  }

  Future<void> _load({bool retryAfterRefresh = true}) async {
    setState(() {
      _loading = true;
      _error = null;
      _events.clear();
      _cursor = null;
    });
    final api = ref.read(apiProvider);
    try {
      final detail = await api.archiveSession(widget.sessionId);
      final page = await api.sessionEvents(
        widget.sessionId,
        view: _digest ? 'digest' : 'transcript',
      );
      if (!mounted) return;
      setState(() {
        _detail = detail;
        _events.addAll(page.events);
        _cursor = page.nextCursor;
        _loading = false;
      });
    } on CormexApiException catch (e) {
      if (!mounted) return;
      if (e.isUnauthorized && retryAfterRefresh) {
        final refreshed = await ref
            .read(connectionProvider.notifier)
            .handleUnauthorized();
        if (!mounted) return;
        if (refreshed) {
          await _load(retryAfterRefresh: false);
          return;
        }
      }
      setState(() {
        _loading = false;
        _error = e.message;
      });
    }
  }

  Future<void> _loadMore() async {
    final cursor = _cursor;
    if (cursor == null || _loadingMore) return;
    setState(() => _loadingMore = true);
    try {
      final page = await ref
          .read(apiProvider)
          .sessionEvents(
            widget.sessionId,
            view: _digest ? 'digest' : 'transcript',
            cursor: cursor,
          );
      if (!mounted) return;
      setState(() {
        _events.addAll(page.events);
        _cursor = page.nextCursor;
        _loadingMore = false;
      });
    } on CormexApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _loadingMore = false;
        _error = e.message;
      });
    }
  }

  /// 把剩下的页都取回来：复制整段对话要的是整段，不是已经滚到的那一截。
  Future<bool> _loadAll() async {
    for (var guard = 0; _cursor != null && guard < 10000; guard++) {
      if (_loadingMore) {
        await Future<void>.delayed(const Duration(milliseconds: 50));
        continue;
      }
      final before = _cursor;
      await _loadMore();
      if (!mounted || _cursor == before) return false;
    }
    return _cursor == null;
  }

  /// 复制整段对话：标题，然后每条消息带上说话人和时间，原文是 Markdown。
  ///
  /// 复制的是当前看到的内容：「只看对话」时只有提问和回答，「查看过程」时
  /// 连同思考、工具调用一起。
  Future<void> _copyAll() async {
    final messenger = ScaffoldMessenger.of(context);
    if (_cursor != null) {
      messenger.showSnackBar(const SnackBar(content: Text('正在取回整段对话…')));
    }
    final complete = await _loadAll();
    if (!mounted) return;
    final agent = _detail?.session.agent;
    final events = _events
        .where(
          (e) => _showProcess
              ? e.kind != TranscriptKind.meta &&
                    e.kind != TranscriptKind.unknown &&
                    e.plainText.trim().isNotEmpty
              : isConversationEvent(e, agent: agent),
        )
        .toList();
    final text = conversationAsText(
      title: _detail?.session.displayTitle,
      events: events,
      agent: agent,
    );
    await copyToClipboard(
      context,
      text,
      done: complete
          ? '已复制整段对话（${events.length} 条）'
          : '只复制了已取回的 ${events.length} 条：后面的没有取回来',
    );
  }

  void _maybeLoadMore() {
    if (!_scroll.hasClients) return;
    final remaining =
        _scroll.position.maxScrollExtent - _scroll.position.pixels;
    if (remaining < 600) _loadMore();
  }

  @override
  Widget build(BuildContext context) {
    final detail = _detail;
    final failed = detail?.projectionStatus == 'failed';
    final shown = _showProcess
        ? _events
        : _events
              .where(
                (e) => isConversationEvent(e, agent: detail?.session.agent),
              )
              .toList();
    final firstUser = _showProcess
        ? shown.indexWhere(
            (e) =>
                e.kind == TranscriptKind.userMessage &&
                isConversationEvent(e, agent: detail?.session.agent),
          )
        : -1;
    final prefix = firstUser > 0
        ? shown.take(firstUser).toList()
        : <TranscriptEvent>[];
    final groupPreparation =
        prefix.isNotEmpty &&
        prefix.every(
          (e) => !isConversationEvent(e, agent: detail?.session.agent),
        );
    final preparation = groupPreparation
        ? prefix
              .where(
                (e) =>
                    e.kind != TranscriptKind.meta &&
                    e.kind != TranscriptKind.unknown,
              )
              .toList()
        : <TranscriptEvent>[];
    final remaining = preparation.isNotEmpty
        ? shown.skip(firstUser).toList()
        : shown;
    if (!_loading &&
        !_loadingMore &&
        _error == null &&
        _cursor != null &&
        shown.length < 8) {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) _loadMore();
      });
    }

    return Scaffold(
      appBar: AppBar(
        automaticallyImplyLeading: !widget.embedded,
        // 页头只放「在哪」：项目名。这段聊天叫什么、多长、什么时候，放在
        // 正文顶上那块标题区里 —— 页头一行塞不下两行的标题，硬塞就是截断
        title: Row(
          children: [
            Icon(
              Icons.folder_outlined,
              size: 16,
              color: Theme.of(context).colorScheme.onSurfaceVariant,
            ),
            const SizedBox(width: 8),
            Flexible(
              child: Text(
                detail == null ? '聊天' : _projectLabel(detail.session),
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: Theme.of(context).textTheme.titleSmall?.copyWith(
                  fontSize: 13,
                  letterSpacing: 0,
                  color: Theme.of(context).colorScheme.onSurfaceVariant,
                ),
              ),
            ),
          ],
        ),
        actions: [
          TextButton.icon(
            onPressed: () => setState(() => _showProcess = !_showProcess),
            icon: Icon(
              _showProcess
                  ? Icons.chat_bubble_outline
                  : Icons.account_tree_outlined,
            ),
            label: Text(_showProcess ? '只看对话' : '查看过程'),
          ),
          IconButton(
            tooltip: '复制整段对话',
            icon: const Icon(Icons.copy_all_outlined),
            onPressed: _loading || _events.isEmpty ? null : _copyAll,
          ),
          IconButton(
            tooltip: _digest ? '显示完整内容' : '只看摘要',
            icon: Icon(_digest ? Icons.unfold_more : Icons.unfold_less),
            onPressed: () {
              setState(() => _digest = !_digest);
              _load();
            },
          ),
          const SizedBox(width: 8),
        ],
      ),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : _error != null && _events.isEmpty
          ? EmptyState(
              icon: Icons.error_outline,
              title: '读不到这段聊天',
              description: _error!,
              action: FilledButton(onPressed: _load, child: const Text('重试')),
            )
          : failed
          ? EmptyState(
              icon: Icons.report_gmailerrorred_outlined,
              title: '这段聊天解析失败',
              description:
                  detail?.projectionError ?? '解析器在这份输入上失败了。原文仍然完整保存，可以稍后重算。',
            )
          : Center(
              child: ConstrainedBox(
                constraints: BoxConstraints(
                  maxWidth: ref.watch(readingPreferencesProvider),
                ),
                // 一个 SelectionArea 包住整个列表：鼠标拖选能跨段落、跨消息，
                // 而不是停在每一段的末尾（见 EventTile 的说明）
                child: SelectionArea(
                  child: ListView.builder(
                    controller: _scroll,
                    padding: const EdgeInsets.only(bottom: 32),
                    itemCount:
                        1 +
                        remaining.length +
                        (preparation.isNotEmpty ? 1 : 0) +
                        (_loadingMore ? 1 : 0),
                    itemBuilder: (context, index) {
                      if (index == 0) {
                        return Column(
                          crossAxisAlignment: CrossAxisAlignment.stretch,
                          children: [
                            _SessionHeader(
                              detail: detail,
                              label: ref
                                  .watch(sessionsControllerProvider)
                                  .sourceName,
                            ),
                            // 不进拖选范围：按钮与 ID 不是对话内容
                            SelectionContainer.disabled(
                              child: SessionMemoryPanel(
                                key: ValueKey('memory-${widget.sessionId}'),
                                sessionId: widget.sessionId,
                              ),
                            ),
                          ],
                        );
                      }
                      final i = index - 1;
                      if (preparation.isNotEmpty && i == 0) {
                        return Theme(
                          data: Theme.of(
                            context,
                          ).copyWith(dividerColor: Colors.transparent),
                          child: ExpansionTile(
                            key: const ValueKey('session-preparation'),
                            leading: const Icon(Icons.layers_outlined),
                            title: const Text('聊天准备信息'),
                            subtitle: Text(
                              '提问前注入的指令和环境信息 · ${preparation.length} 项',
                            ),
                            children: [
                              for (final event in preparation)
                                EventTile(
                                  event: event,
                                  agent: detail?.session.agent,
                                ),
                            ],
                          ),
                        );
                      }
                      final eventIndex = i - (preparation.isNotEmpty ? 1 : 0);
                      if (eventIndex >= remaining.length) {
                        return const Padding(
                          padding: EdgeInsets.all(16),
                          child: Center(child: CircularProgressIndicator()),
                        );
                      }
                      return EventTile(
                        event: remaining[eventIndex],
                        agent: detail?.session.agent,
                      );
                    },
                  ),
                ),
              ),
            ),
    );
  }
}

String _projectLabel(ArchiveSession s) {
  final name = s.projectName?.trim();
  if (name != null && name.isNotEmpty) return name;
  return ArchiveProject(key: s.projectKey, chatCount: 0).displayName;
}

/// 正文顶上的标题区：这段聊天叫什么、来自哪、跨了多久、多长。
class _SessionHeader extends StatelessWidget {
  const _SessionHeader({required this.detail, required this.label});
  final ArchiveSessionDetail? detail;
  final String Function(String) label;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final session = detail?.session;
    if (session == null) return const SizedBox(height: 16);
    final span = session.firstAt == null || session.lastAt == null
        ? null
        : formatDate(session.firstAt) == formatDate(session.lastAt)
        ? formatAbsolute(session.firstAt)
        : '${formatDate(session.firstAt)} – ${formatDate(session.lastAt)}';
    final facts = [
      ?span,
      session.messageCount == null ? '消息数统计中' : '${session.messageCount} 条消息',
      '${session.eventCount} 个事件',
    ];
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 28, 16, 18),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Container(
                width: 7,
                height: 7,
                decoration: BoxDecoration(
                  color: Agents.dot(session.agent, theme.brightness),
                  shape: BoxShape.circle,
                ),
              ),
              const SizedBox(width: 7),
              Text(
                label(session.agent),
                style: theme.textTheme.labelMedium?.copyWith(
                  color: theme.colorScheme.onSurfaceVariant,
                  fontWeight: FontWeight.w600,
                ),
              ),
            ],
          ),
          const SizedBox(height: 10),
          Text(
            session.displayTitle,
            style: theme.textTheme.headlineSmall?.copyWith(
              fontSize: 22,
              height: 1.35,
              fontWeight: FontWeight.w600,
              letterSpacing: -0.3,
            ),
          ),
          const SizedBox(height: 10),
          Text(
            facts.join(' · '),
            style: theme.textTheme.bodySmall?.copyWith(
              fontFeatures: const [FontFeature.tabularFigures()],
            ),
          ),
          if (session.sourceArchived) ...[
            const SizedBox(height: 6),
            const ArchiveSourceLabel(),
          ],
          const SizedBox(height: 18),
          Divider(color: theme.colorScheme.outlineVariant),
        ],
      ),
    );
  }
}

/// 一段对话的纯文本：标题，然后每条消息一节，说话人、绝对时间、原文。
String conversationAsText({
  required String? title,
  required List<TranscriptEvent> events,
  required String? agent,
}) {
  final out = StringBuffer();
  if (title != null && title.trim().isNotEmpty) {
    out
      ..writeln('# ${title.trim()}')
      ..writeln();
  }
  for (final (i, e) in events.indexed) {
    if (i > 0) {
      out
        ..writeln()
        ..writeln('---')
        ..writeln();
    }
    final who = switch (e.kind) {
      TranscriptKind.userMessage => '你',
      TranscriptKind.agentMessage => Agents.speaker(agent),
      TranscriptKind.agentThought => '思考',
      TranscriptKind.toolCall => '调用 ${e.toolName ?? ''}'.trim(),
      TranscriptKind.toolResult => '工具结果',
      TranscriptKind.plan => '计划',
      _ => '系统',
    };
    final at = e.occurredAt == null ? '' : ' · ${formatAbsolute(e.occurredAt)}';
    out
      ..writeln('**$who**$at')
      ..writeln()
      ..writeln(e.plainText.trim());
  }
  return out.toString();
}
