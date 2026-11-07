import 'dart:async';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../../core/agents.dart';
import '../../core/formatting.dart';
import '../../models/archive_session.dart';
import '../../state/sessions_controller.dart';
import '../../widgets/empty_state.dart';
import '../home/home_page.dart';
import '../settings/settings_page.dart';
import 'import_dialog.dart';
import 'session_detail_page.dart';
import 'widgets/archive_source_label.dart';

/// 聊天档案：左边「项目 → 聊天」，右边读选中的那一段。
///
/// 左栏是一张**目录**，不是一叠卡片：项目是分组标题，聊天是它下面的行。
/// 每个项目各包一张卡片时，十几个项目就是十几个同样圆角同样底色的块，
/// 视线找不到落点；而目录的层级本来就靠缩进和字重就能表达。
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

  bool get _wide => MediaQuery.sizeOf(context).width >= HomePage.wideBreakpoint;

  @override
  Widget build(BuildContext context) {
    final state = ref.watch(sessionsControllerProvider);
    final notifier = ref.read(sessionsControllerProvider.notifier);
    final theme = Theme.of(context);
    final wide = _wide;
    final chats = state.projects.fold<int>(0, (n, p) => n + p.chatCount);

    final directory = Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 14, 16, 8),
          child: TextField(
            controller: _search,
            onChanged: _searchChanged,
            decoration: InputDecoration(
              hintText: '搜索聊天或项目',
              prefixIcon: const Icon(Icons.search, size: 20),
              suffixIcon: _search.text.isEmpty
                  ? null
                  : IconButton(
                      tooltip: '清除搜索',
                      icon: const Icon(Icons.close, size: 18),
                      onPressed: () {
                        _search.clear();
                        _searchChanged('');
                      },
                    ),
            ),
          ),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(12, 0, 16, 6),
          child: Row(
            children: [
              _AgentFilter(
                agent: state.agent,
                sources: state.sources,
                label: state.sourceName,
                onChanged: _changeAgent,
              ),
              const Spacer(),
              if (!state.loading && state.error == null)
                Text(
                  '${state.projects.length} 个项目 · $chats 个聊天',
                  style: theme.textTheme.labelSmall?.copyWith(
                    fontFeatures: const [FontFeature.tabularFigures()],
                  ),
                ),
            ],
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
                    onPressed: () =>
                        notifier.load(agent: state.agent, query: state.query),
                    child: const Text('重试'),
                  ),
                )
              : state.projects.isEmpty
              ? EmptyState(
                  icon: Icons.inbox_outlined,
                  title: state.query.isEmpty ? '此来源暂无聊天' : '没有找到匹配的聊天',
                  description: '项目只显示已有聊天的项目。',
                )
              : ListView.builder(
                  key: const ValueKey('project-list'),
                  padding: const EdgeInsets.fromLTRB(8, 2, 8, 24),
                  itemCount: state.projects.length,
                  itemBuilder: (context, i) {
                    final project = state.projects[i];
                    return _ProjectGroup(
                      key: ObjectKey(project),
                      label: state.sourceName,
                      project: project,
                      group: state.groups[project.key] ?? const ProjectChats(),
                      selectedId: _selectedId,
                      onOpen: _openSession,
                    );
                  },
                ),
        ),
      ],
    );

    final actions = [
      IconButton(
        tooltip: '导入聊天记录',
        icon: const Icon(Icons.upload_file_outlined),
        onPressed: () async {
          final imported = await showDialog<bool>(
            context: context,
            builder: (_) => const ImportArchiveDialog(),
          );
          if (imported == true) {
            _debounce?.cancel();
            notifier.load(agent: state.agent, query: _search.text);
          }
        },
      ),
      IconButton(
        tooltip: '刷新',
        icon: const Icon(Icons.refresh),
        onPressed: () {
          _debounce?.cancel();
          notifier.load(agent: state.agent, query: _search.text);
        },
      ),
      IconButton(
        tooltip: '设置',
        icon: const Icon(Icons.settings_outlined),
        onPressed: () => Navigator.of(
          context,
        ).push(MaterialPageRoute<void>(builder: (_) => const SettingsPage())),
      ),
      const SizedBox(width: 8),
    ];

    if (!wide) {
      return Scaffold(
        appBar: AppBar(title: const Text('聊天档案'), actions: actions),
        body: directory,
      );
    }

    // 宽屏：页头不横跨两栏。目录栏自带一条与右侧阅读区页头同高的标题，
    // 两条页头并排对齐，而不是上下叠成两层
    return Scaffold(
      body: Row(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          SizedBox(
            width: 360,
            child: ColoredBox(
              color: theme.colorScheme.surfaceContainerLowest,
              child: ClipRect(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    Container(
                      height: theme.appBarTheme.toolbarHeight ?? 60,
                      padding: const EdgeInsets.only(left: 20),
                      decoration: BoxDecoration(
                        border: Border(
                          bottom: BorderSide(
                            color: theme.colorScheme.outlineVariant,
                          ),
                        ),
                      ),
                      child: Row(
                        children: [
                          Expanded(
                            child: Text(
                              '聊天档案',
                              style: theme.appBarTheme.titleTextStyle,
                            ),
                          ),
                          ...actions,
                        ],
                      ),
                    ),
                    Expanded(child: directory),
                  ],
                ),
              ),
            ),
          ),
          VerticalDivider(width: 1, color: theme.colorScheme.outlineVariant),
          Expanded(
            child: _selectedId == null
                ? const EmptyState(
                    icon: Icons.forum_outlined,
                    title: '选择聊天',
                    description: '展开左侧项目，选择聊天后查看完整对话。',
                  )
                : SessionDetailPage(
                    key: ValueKey(_selectedId),
                    sessionId: _selectedId!,
                    embedded: true,
                  ),
          ),
        ],
      ),
    );
  }

  void _openSession(ArchiveSession session) {
    if (_wide) {
      setState(() => _selectedId = session.id);
    } else {
      Navigator.of(context).push(
        MaterialPageRoute<void>(
          builder: (_) => SessionDetailPage(sessionId: session.id),
        ),
      );
    }
  }
}

/// 来源筛选。一枚小筹码而不是整行下拉框：它是次要的过滤条件，
/// 大多数时候停在「全部来源」，不该和搜索框抢同样的分量。
///
/// 只列**有会话的**来源（服务端 `/archive/sources`），按厂商分组，带数量。
/// 以前列的是 Cormex 认识的全部 agent，对任何一个人来说大半是空的。
class _AgentFilter extends StatelessWidget {
  const _AgentFilter({
    required this.agent,
    required this.sources,
    required this.label,
    required this.onChanged,
  });
  final String? agent;
  final List<ArchiveSource> sources;
  final String Function(String) label;
  final ValueChanged<String?> onChanged;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final items = <PopupMenuEntry<String>>[
      const PopupMenuItem(value: '', child: Text('全部来源')),
    ];
    String? lastProvider;
    for (final s in sources) {
      final provider = s.provider ?? '其他';
      if (provider != lastProvider) {
        items.add(const PopupMenuDivider());
        items.add(
          PopupMenuItem<String>(
            enabled: false,
            height: 28,
            child: Text(
              provider,
              style: theme.textTheme.labelSmall?.copyWith(
                letterSpacing: 0.4,
                fontWeight: FontWeight.w600,
              ),
            ),
          ),
        );
        lastProvider = provider;
      }
      items.add(
        PopupMenuItem(
          value: s.agent,
          child: Row(
            children: [
              _Dot(color: Agents.dot(s.agent, theme.brightness)),
              const SizedBox(width: 10),
              Expanded(child: Text(s.name)),
              const SizedBox(width: 12),
              Text(
                '${s.kind == 'chat' ? '聊天' : '编程'} · ${s.chatCount}',
                style: theme.textTheme.labelSmall?.copyWith(
                  fontFeatures: const [FontFeature.tabularFigures()],
                ),
              ),
            ],
          ),
        ),
      );
    }
    return PopupMenuButton<String>(
      tooltip: '按来源筛选',
      initialValue: agent ?? '',
      onSelected: (v) => onChanged(v.isEmpty ? null : v),
      itemBuilder: (context) => items,
      child: Container(
        padding: const EdgeInsets.fromLTRB(10, 5, 6, 5),
        decoration: BoxDecoration(
          color: agent == null
              ? Colors.transparent
              : theme.colorScheme.primaryContainer,
          border: Border.all(
            color: agent == null
                ? theme.colorScheme.outlineVariant
                : Colors.transparent,
          ),
          borderRadius: BorderRadius.circular(8),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (agent != null) ...[
              _Dot(color: Agents.dot(agent!, theme.brightness)),
              const SizedBox(width: 6),
            ],
            Text(
              agent == null ? '全部来源' : label(agent!),
              style: theme.textTheme.labelMedium?.copyWith(
                color: agent == null
                    ? theme.colorScheme.onSurfaceVariant
                    : theme.colorScheme.onPrimaryContainer,
                fontWeight: FontWeight.w600,
              ),
            ),
            Icon(
              Icons.expand_more,
              size: 18,
              color: theme.colorScheme.onSurfaceVariant,
            ),
          ],
        ),
      ),
    );
  }
}

class _Dot extends StatelessWidget {
  const _Dot({required this.color});
  final Color color;
  @override
  Widget build(BuildContext context) => Container(
    width: 7,
    height: 7,
    decoration: BoxDecoration(color: color, shape: BoxShape.circle),
  );
}

class _ProjectGroup extends ConsumerStatefulWidget {
  const _ProjectGroup({
    super.key,
    required this.label,
    required this.project,
    required this.group,
    required this.onOpen,
    this.selectedId,
  });
  final String Function(String) label;
  final ArchiveProject project;
  final ProjectChats group;
  final ValueChanged<ArchiveSession> onOpen;
  final String? selectedId;

  @override
  ConsumerState<_ProjectGroup> createState() => _ProjectGroupState();
}

class _ProjectGroupState extends ConsumerState<_ProjectGroup> {
  bool _expanded = false;

  void _toggle() {
    setState(() => _expanded = !_expanded);
    if (_expanded) {
      ref
          .read(sessionsControllerProvider.notifier)
          .loadProject(widget.project.key);
    }
  }

  @override
  Widget build(BuildContext context) {
    final notifier = ref.read(sessionsControllerProvider.notifier);
    final theme = Theme.of(context);
    final project = widget.project;
    final group = widget.group;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Material(
          type: MaterialType.transparency,
          child: InkWell(
            borderRadius: BorderRadius.circular(8),
            onTap: _toggle,
            child: Padding(
              padding: const EdgeInsets.fromLTRB(6, 9, 10, 9),
              child: Row(
                children: [
                  AnimatedRotation(
                    turns: _expanded ? 0.25 : 0,
                    duration: const Duration(milliseconds: 150),
                    child: Icon(
                      Icons.chevron_right,
                      size: 18,
                      color: theme.colorScheme.onSurfaceVariant,
                    ),
                  ),
                  const SizedBox(width: 4),
                  Icon(
                    project.key == null
                        ? Icons.inbox_outlined
                        : (_expanded
                              ? Icons.folder_open_outlined
                              : Icons.folder_outlined),
                    size: 18,
                    color: theme.colorScheme.onSurfaceVariant,
                  ),
                  const SizedBox(width: 10),
                  Expanded(
                    child: Tooltip(
                      message: project.key ?? '未分类',
                      child: Text(
                        project.displayName,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: theme.textTheme.titleMedium?.copyWith(
                          fontSize: 14,
                        ),
                      ),
                    ),
                  ),
                  const SizedBox(width: 8),
                  Text(
                    '${project.chatCount} 个聊天',
                    style: theme.textTheme.labelSmall?.copyWith(
                      fontFeatures: const [FontFeature.tabularFigures()],
                    ),
                  ),
                ],
              ),
            ),
          ),
        ),
        if (_expanded) ...[
          for (final session in group.sessions)
            _SessionTile(
              key: ValueKey(session.id),
              label: widget.label(session.agent),
              session: session,
              onOpen: widget.onOpen,
              selected: session.id == widget.selectedId,
            ),
          if (group.loading)
            const Padding(
              padding: EdgeInsets.fromLTRB(36, 10, 12, 10),
              child: LinearProgressIndicator(minHeight: 2),
            ),
          if (group.error != null)
            Padding(
              padding: const EdgeInsets.fromLTRB(36, 6, 12, 6),
              child: Row(
                children: [
                  Expanded(
                    child: Text(
                      group.error!,
                      style: theme.textTheme.bodySmall?.copyWith(
                        color: theme.colorScheme.error,
                      ),
                    ),
                  ),
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
            Padding(
              padding: const EdgeInsets.only(left: 28),
              child: Align(
                alignment: Alignment.centerLeft,
                child: TextButton(
                  onPressed: () =>
                      notifier.loadProject(project.key, more: true),
                  child: const Text('加载更多聊天'),
                ),
              ),
            ),
          if (group.loaded && group.sessions.isEmpty && group.error == null)
            Padding(
              padding: const EdgeInsets.fromLTRB(36, 6, 12, 10),
              child: Text('没有匹配的聊天，刷新目录后重试。', style: theme.textTheme.bodySmall),
            ),
          const SizedBox(height: 6),
        ],
      ],
    );
  }
}

class _SessionTile extends StatelessWidget {
  const _SessionTile({
    super.key,
    required this.label,
    required this.session,
    required this.onOpen,
    required this.selected,
  });

  final String label;
  final ArchiveSession session;
  final ValueChanged<ArchiveSession> onOpen;
  final bool selected;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final meta = theme.textTheme.labelSmall?.copyWith(
      fontFeatures: const [FontFeature.tabularFigures()],
    );
    return Padding(
      padding: const EdgeInsets.only(left: 24, bottom: 2),
      child: Material(
        color: selected
            ? scheme.primaryContainer.withValues(alpha: 0.6)
            : Colors.transparent,
        borderRadius: BorderRadius.circular(8),
        child: InkWell(
          borderRadius: BorderRadius.circular(8),
          onTap: () => onOpen(session),
          child: Padding(
            padding: const EdgeInsets.fromLTRB(12, 9, 10, 9),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Expanded(
                      child: Text(
                        session.displayTitle,
                        // 两行而不是一行：聊天的标题是第一句提问，前十几个字
                        // 往往还是「帮我看一下」，真正的主题在后半句
                        maxLines: 2,
                        overflow: TextOverflow.ellipsis,
                        style: theme.textTheme.bodyMedium?.copyWith(
                          height: 1.4,
                          fontWeight: FontWeight.w500,
                          color: selected
                              ? scheme.onPrimaryContainer
                              : scheme.onSurface,
                        ),
                      ),
                    ),
                    if (session.soleCopy)
                      Tooltip(
                        message:
                            '宿主已经删掉了原始文件，Cormex 手里是唯一一份。'
                            '在这里删除就是删掉最后一份。',
                        child: Padding(
                          padding: const EdgeInsets.only(left: 6, top: 2),
                          child: Icon(
                            Icons.warning_amber_rounded,
                            size: 16,
                            color: scheme.error,
                          ),
                        ),
                      ),
                  ],
                ),
                const SizedBox(height: 5),
                Row(
                  children: [
                    _Dot(color: Agents.dot(session.agent, theme.brightness)),
                    const SizedBox(width: 6),
                    Flexible(
                      child: Text(
                        [
                          label,
                          session.messageCount == null
                              ? '消息数统计中'
                              : '${session.messageCount} 条消息',
                        ].join(' · '),
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: meta,
                      ),
                    ),
                    const SizedBox(width: 8),
                    if (session.lastAt != null)
                      Text(formatCompactDate(session.lastAt), style: meta),
                  ],
                ),
                if (session.sourceArchived) ...[
                  const SizedBox(height: 3),
                  const ArchiveSourceLabel(),
                ],
              ],
            ),
          ),
        ),
      ),
    );
  }
}
