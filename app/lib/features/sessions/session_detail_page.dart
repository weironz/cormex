import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../api/api_exception.dart';
import '../../models/archive_session.dart';
import '../../models/transcript_event.dart';
import '../../state/connection_controller.dart';
import '../../state/reading_preferences.dart';
import '../../widgets/empty_state.dart';
import 'widgets/event_tile.dart';
import 'widgets/archive_source_label.dart';

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
  const SessionDetailPage({super.key, required this.sessionId});

  final String sessionId;

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
        title: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              detail?.session.displayTitle ?? '聊天',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
            if (detail?.session.sourceArchived == true)
              const ArchiveSourceLabel(),
            if (detail != null)
              Text(
                '${detail.session.messageCount == null ? '消息数统计中' : '${detail.session.messageCount} 条消息'} · ${detail.session.eventCount} 个事件',
                style: Theme.of(context).textTheme.labelSmall,
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
            tooltip: _digest ? '显示完整内容' : '只看摘要',
            icon: Icon(_digest ? Icons.unfold_more : Icons.unfold_less),
            onPressed: () {
              setState(() => _digest = !_digest);
              _load();
            },
          ),
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
                child: ListView.builder(
                  controller: _scroll,
                  padding: const EdgeInsets.symmetric(vertical: 8),
                  itemCount:
                      remaining.length +
                      (preparation.isNotEmpty ? 1 : 0) +
                      (_loadingMore ? 1 : 0),
                  itemBuilder: (context, i) {
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
    );
  }
}
