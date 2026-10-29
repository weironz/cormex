import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../core/formatting.dart';
import '../../models/archive_session.dart';
import '../../state/sessions_controller.dart';
import '../../widgets/empty_state.dart';
import 'session_detail_page.dart';

/// 采集进来的会话列表。
///
/// # 与记忆浏览器并列，不是它的子页
///
/// 「我的记忆里有什么」和「我的 agent 到底说了什么」是两个问题。前者是
/// 抽取之后的事实，后者是原始对话 —— 而「凭什么这么认为」的答案在后者里。
/// 把它们做成父子页会让第二个问题永远藏在第一个后面。
class SessionsPage extends ConsumerStatefulWidget {
  const SessionsPage({super.key});

  @override
  ConsumerState<SessionsPage> createState() => _SessionsPageState();
}

class _SessionsPageState extends ConsumerState<SessionsPage> {
  final ScrollController _scroll = ScrollController();

  @override
  void initState() {
    super.initState();
    _scroll.addListener(_maybeLoadMore);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      ref.read(sessionsControllerProvider.notifier).load();
    });
  }

  @override
  void dispose() {
    _scroll.dispose();
    super.dispose();
  }

  void _maybeLoadMore() {
    if (!_scroll.hasClients) return;
    // 提前一屏开始加载，滚动才不会在底部停一下
    final remaining = _scroll.position.maxScrollExtent - _scroll.position.pixels;
    if (remaining < 400) {
      ref.read(sessionsControllerProvider.notifier).loadMore();
    }
  }

  @override
  Widget build(BuildContext context) {
    final state = ref.watch(sessionsControllerProvider);
    final notifier = ref.read(sessionsControllerProvider.notifier);

    return Scaffold(
      appBar: AppBar(
        title: const Text('会话档案'),
        actions: [
          PopupMenuButton<String?>(
            tooltip: '按 agent 筛选',
            icon: const Icon(Icons.filter_list),
            onSelected: (agent) => notifier.load(agent: agent),
            itemBuilder: (context) => [
              const PopupMenuItem(value: null, child: Text('全部 agent')),
              for (final agent in _agentsIn(state.sessions))
                PopupMenuItem(value: agent, child: Text(agent)),
            ],
          ),
          IconButton(
            tooltip: '刷新',
            icon: const Icon(Icons.refresh),
            onPressed: () => notifier.load(agent: state.agent),
          ),
        ],
      ),
      body: switch (state) {
        SessionsState(loading: true) => const Center(
          child: CircularProgressIndicator(),
        ),
        SessionsState(error: final e?) => EmptyState(
          icon: Icons.error_outline,
          title: '读不到会话',
          description: e,
          action: FilledButton(
            onPressed: () => notifier.load(agent: state.agent),
            child: const Text('重试'),
          ),
        ),
        SessionsState(sessions: final s) when s.isEmpty => const EmptyState(
          icon: Icons.inbox_outlined,
          title: '还没有采集到会话',
          description:
              '在装了 agent 的机器上运行 `cormex login` 和 `cormex init`，'
              '会话就会出现在这里。',
        ),
        _ => ListView.separated(
          controller: _scroll,
          itemCount: state.sessions.length + (state.loadingMore ? 1 : 0),
          separatorBuilder: (_, _) => const Divider(height: 1),
          itemBuilder: (context, i) {
            if (i >= state.sessions.length) {
              return const Padding(
                padding: EdgeInsets.all(16),
                child: Center(child: CircularProgressIndicator()),
              );
            }
            return _SessionTile(session: state.sessions[i]);
          },
        ),
      },
    );
  }

  /// 列表里出现过的 agent，用来填筛选菜单。
  ///
  /// 从**当前结果**里取而不是从 catalog 取：一个菜单项点了之后没有任何结果，
  /// 比菜单里少一项更让人困惑。
  List<String> _agentsIn(List<ArchiveSession> sessions) {
    final set = <String>{for (final s in sessions) s.agent};
    return set.toList()..sort();
  }
}

class _SessionTile extends StatelessWidget {
  const _SessionTile({required this.session});

  final ArchiveSession session;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return ListTile(
      title: Row(
        children: [
          Expanded(
            child: Text(
              session.displayTitle,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: theme.textTheme.titleSmall,
            ),
          ),
          if (session.soleCopy)
            Tooltip(
              message:
                  '宿主已经删掉了原始文件，Cormex 手里是唯一一份。'
                  '在这里删除就是删掉最后一份。',
              child: Padding(
                padding: const EdgeInsets.only(left: 8),
                child: Icon(
                  Icons.warning_amber_rounded,
                  size: 18,
                  color: theme.colorScheme.error,
                ),
              ),
            ),
        ],
      ),
      subtitle: Text(
        [
          session.agent,
          if (session.projectKey != null) session.projectKey!,
          '${session.eventCount} 条',
          if (session.lastAt != null) formatDate(session.lastAt),
        ].join(' · '),
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
      ),
      trailing: const Icon(Icons.chevron_right),
      onTap: () => Navigator.of(context).push(
        MaterialPageRoute<void>(
          builder: (_) => SessionDetailPage(sessionId: session.id),
        ),
      ),
    );
  }
}
