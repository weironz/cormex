import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../api/api_exception.dart';
import '../../models/archive_session.dart';
import '../../models/transcript_event.dart';
import '../../state/connection_controller.dart';
import '../../widgets/empty_state.dart';
import 'widgets/event_tile.dart';

/// 一段会话的对话视图。
///
/// # 虚拟列表 + 滚到底翻页
///
/// 一段会话可以有一千多条事件，全部一次性渲染会让滚动卡死。这里用
/// `ListView.builder`（只构建可见的那几条），并在接近底部时按游标取下一页。
///
/// # 解析失败要说出来
///
/// `projection_status == failed` 时显示原因，而不是一段空对话 —— 空对话
/// 读起来像「这段会话没内容」，而实际是「解析器在这份输入上失败了」。
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

  Future<void> _load() async {
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
    final remaining = _scroll.position.maxScrollExtent - _scroll.position.pixels;
    if (remaining < 600) _loadMore();
  }

  @override
  Widget build(BuildContext context) {
    final detail = _detail;
    final failed = detail?.projectionStatus == 'failed';

    return Scaffold(
      appBar: AppBar(
        title: Text(detail?.session.displayTitle ?? '会话'),
        actions: [
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
              title: '读不到这段会话',
              description: _error!,
              action: FilledButton(
                onPressed: _load,
                child: const Text('重试'),
              ),
            )
          : failed
          ? EmptyState(
              icon: Icons.report_gmailerrorred_outlined,
              title: '这段会话解析失败',
              description:
                  detail?.projectionError ??
                  '解析器在这份输入上失败了。原文仍然完整保存，可以稍后重算。',
            )
          : ListView.builder(
              controller: _scroll,
              padding: const EdgeInsets.symmetric(vertical: 8),
              itemCount: _events.length + (_loadingMore ? 1 : 0),
              itemBuilder: (context, i) {
                if (i >= _events.length) {
                  return const Padding(
                    padding: EdgeInsets.all(16),
                    child: Center(child: CircularProgressIndicator()),
                  );
                }
                return EventTile(event: _events[i]);
              },
            ),
    );
  }
}
