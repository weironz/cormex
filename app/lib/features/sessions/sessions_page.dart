import 'dart:async';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../../core/formatting.dart';
import '../../models/archive_session.dart';
import '../../state/sessions_controller.dart';
import '../../widgets/empty_state.dart';
import '../settings/settings_page.dart';
import 'session_detail_page.dart';
import 'widgets/archive_source_label.dart';

class SessionsPage extends ConsumerStatefulWidget {
  const SessionsPage({super.key});
  @override
  ConsumerState<SessionsPage> createState() => _SessionsPageState();
}

class _SessionsPageState extends ConsumerState<SessionsPage> {
  final TextEditingController _search = TextEditingController();
  Timer? _debounce;
  String? _selectedId;
  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) ref.read(sessionsControllerProvider.notifier).load();
    });
  }

  @override
  void dispose() {
    _debounce?.cancel();
    _search.dispose();
    super.dispose();
  }

  void _searchChanged(String value) {
    _debounce?.cancel();
    _debounce = Timer(const Duration(milliseconds: 300), () {
      if (!mounted) return;
      ref
          .read(sessionsControllerProvider.notifier)
          .load(
            agent: ref.read(sessionsControllerProvider).agent,
            query: value,
          );
    });
    setState(() {});
  }

  void _changeAgent(String? agent) {
    _debounce?.cancel();
    setState(() => _selectedId = null);
    ref
        .read(sessionsControllerProvider.notifier)
        .load(agent: agent, query: _search.text);
  }

  @override
  Widget build(BuildContext context) {
    final state = ref.watch(sessionsControllerProvider);
    final notifier = ref.read(sessionsControllerProvider.notifier);
    final wide = MediaQuery.sizeOf(context).width >= 1000;
    return Scaffold(
      appBar: AppBar(
        title: const Text('聊天档案'),
        actions: [
          IconButton(
            tooltip: '设置',
            icon: const Icon(Icons.settings_outlined),
            onPressed: () => Navigator.of(context).push(
              MaterialPageRoute<void>(builder: (_) => const SettingsPage()),
            ),
          ),
          IconButton(
            tooltip: '刷新',
            icon: const Icon(Icons.refresh),
            onPressed: () {
              _debounce?.cancel();
              notifier.load(agent: state.agent, query: _search.text);
            },
          ),
        ],
      ),
      body: Row(
        children: [
          SizedBox(
            width: wide ? 360 : MediaQuery.sizeOf(context).width,
            child: ClipRect(
              child: Column(
                children: [
                  Padding(
                    padding: const EdgeInsets.all(16),
                    child: DropdownButtonFormField<String>(
                      key: ValueKey(state.agent),
                      initialValue: state.agent ?? '',
                      isExpanded: true,
                      decoration: const InputDecoration(
                        labelText: '来源',
                        border: OutlineInputBorder(),
                        isDense: true,
                      ),
                      items: [
                        const DropdownMenuItem(value: '', child: Text('全部来源')),
                        for (final agent in _agents)
                          DropdownMenuItem(
                            value: agent,
                            child: Text(_agentLabel(agent)),
                          ),
                      ],
                      onChanged: (value) =>
                          _changeAgent(value == '' ? null : value),
                    ),
                  ),
                  Padding(
                    padding: const EdgeInsets.fromLTRB(16, 0, 16, 12),
                    child: TextField(
                      controller: _search,
                      onChanged: _searchChanged,
                      decoration: InputDecoration(
                        hintText: '搜索聊天或项目',
                        prefixIcon: const Icon(Icons.search),
                        border: const OutlineInputBorder(),
                        isDense: true,
                        suffixIcon: _search.text.isEmpty
                            ? null
                            : IconButton(
                                tooltip: '清除搜索',
                                icon: const Icon(Icons.close),
                                onPressed: () {
                                  _search.clear();
                                  _searchChanged('');
                                },
                              ),
                      ),
                    ),
                  ),
                  Expanded(
                    child: state.loading
                        ? const Center(child: CircularProgressIndicator())
                        : state.error != null
                        ? EmptyState(
                            icon: Icons.error_outline,
                            title: '读不到项目',
                            description: state.error!,
                            action: FilledButton(
                              onPressed: () => notifier.load(
                                agent: state.agent,
                                query: state.query,
                              ),
                              child: const Text('重试'),
                            ),
                          )
                        : state.projects.isEmpty
                        ? EmptyState(
                            icon: Icons.inbox_outlined,
                            title: state.query.isEmpty
                                ? '此来源暂无聊天'
                                : '没有找到匹配的聊天',
                            description: '项目只显示已有聊天的项目。',
                          )
                        : ListView.builder(
                            key: const ValueKey('project-list'),
                            padding: const EdgeInsets.fromLTRB(12, 0, 12, 24),
                            itemCount: state.projects.length,
                            itemBuilder: (context, i) {
                              final project = state.projects[i];
                              return _ProjectGroup(
                                key: ObjectKey(project),
                                project: project,
                                group:
                                    state.groups[project.key] ??
                                    const ProjectChats(),
                                selectedId: _selectedId,
                                onOpen: _openSession,
                              );
                            },
                          ),
                  ),
                ],
              ),
            ),
          ),
          if (wide)
            Expanded(
              child: Container(
                decoration: BoxDecoration(
                  border: Border(
                    left: BorderSide(
                      color: Theme.of(context).colorScheme.outlineVariant,
                    ),
                  ),
                ),
                child: _selectedId == null
                    ? const EmptyState(
                        icon: Icons.forum_outlined,
                        title: '选择聊天',
                        description: '展开左侧项目，选择聊天后查看完整对话。',
                      )
                    : SessionDetailPage(
                        key: ValueKey(_selectedId),
                        sessionId: _selectedId!,
                      ),
              ),
            ),
        ],
      ),
    );
  }

  void _openSession(ArchiveSession session) {
    if (MediaQuery.sizeOf(context).width >= 1000) {
      setState(() => _selectedId = session.id);
    } else {
      Navigator.of(context).push(
        MaterialPageRoute<void>(
          builder: (_) => SessionDetailPage(sessionId: session.id),
        ),
      );
    }
  }

  static const _agents = [
    'codex',
    'chatgpt_export',
    'claude_code',
    'gemini_cli',
    'opencode',
    'cursor',
    'copilot_cli',
    'qwen_code',
    'copilot_chat',
    'cline',
    'roo_code',
    'aider',
    'goose',
    'claude_export',
  ];
  String _agentLabel(String agent) => switch (agent) {
    'codex' => 'Codex 编程',
    'chatgpt_export' => 'ChatGPT 聊天',
    'claude_code' => 'Claude Code',
    'gemini_cli' => 'Gemini CLI',
    'opencode' => 'OpenCode',
    'cursor' => 'Cursor',
    _ => agent,
  };
}

class _ProjectGroup extends ConsumerWidget {
  const _ProjectGroup({
    super.key,
    required this.project,
    required this.group,
    required this.onOpen,
    this.selectedId,
  });
  final ArchiveProject project;
  final ProjectChats group;
  final ValueChanged<ArchiveSession> onOpen;
  final String? selectedId;
  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final notifier = ref.read(sessionsControllerProvider.notifier);
    return Card(
      elevation: 0,
      margin: const EdgeInsets.only(bottom: 8),
      clipBehavior: Clip.antiAlias,
      child: ExpansionTile(
        shape: const Border(),
        collapsedShape: const Border(),
        leading: Icon(
          project.key == null ? Icons.inbox_outlined : Icons.folder_outlined,
          size: 20,
        ),
        title: Tooltip(
          message: project.key ?? '未分类',
          child: Text(project.displayName),
        ),
        subtitle: Text('${project.chatCount} 个聊天'),
        onExpansionChanged: (expanded) {
          if (expanded) notifier.loadProject(project.key);
        },
        children: [
          for (final session in group.sessions)
            _SessionTile(
              key: ValueKey(session.id),
              session: session,
              onOpen: onOpen,
              selected: session.id == selectedId,
            ),
          if (group.loading)
            const Padding(
              padding: EdgeInsets.all(16),
              child: LinearProgressIndicator(),
            ),
          if (group.error != null)
            Padding(
              padding: const EdgeInsets.all(12),
              child: Column(
                children: [
                  Text(group.error!),
                  TextButton(
                    onPressed: () =>
                        notifier.loadProject(project.key, more: group.loaded),
                    child: const Text('重试'),
                  ),
                ],
              ),
            ),
          if (group.loaded &&
              !group.loading &&
              group.error == null &&
              group.cursor != null)
            TextButton(
              onPressed: () => notifier.loadProject(project.key, more: true),
              child: const Text('加载更多聊天'),
            ),
          if (group.loaded && group.sessions.isEmpty && group.error == null)
            const Padding(
              padding: EdgeInsets.all(12),
              child: Text('没有匹配的聊天，刷新目录后重试。'),
            ),
        ],
      ),
    );
  }
}

class _SessionTile extends StatelessWidget {
  const _SessionTile({
    super.key,
    required this.session,
    required this.onOpen,
    required this.selected,
  });

  final ArchiveSession session;
  final ValueChanged<ArchiveSession> onOpen;
  final bool selected;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return ListTile(
      selected: selected,
      selectedTileColor: theme.colorScheme.primaryContainer.withValues(
        alpha: 0.35,
      ),
      contentPadding: const EdgeInsets.fromLTRB(56, 0, 16, 4),
      leading: Icon(
        Icons.chat_bubble_outline_rounded,
        size: 18,
        color: theme.colorScheme.onSurfaceVariant,
      ),
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
      subtitle: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            [
              switch (session.agent) {
                'codex' => 'Codex 编程',
                'chatgpt_export' => 'ChatGPT 聊天',
                _ => session.agent,
              },
              session.messageCount == null
                  ? '消息数统计中'
                  : '${session.messageCount} 条消息',
              if (session.lastAt != null) formatDate(session.lastAt),
            ].join(' · '),
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
          ),
          if (session.sourceArchived) const ArchiveSourceLabel(),
        ],
      ),
      trailing: const Icon(Icons.chevron_right),
      onTap: () => onOpen(session),
    );
  }
}
