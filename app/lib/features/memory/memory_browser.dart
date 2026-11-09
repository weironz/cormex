import 'package:flutter/material.dart';
import '../home/home_page.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../core/formatting.dart';
import '../../models/memory_artifact.dart';
import '../../models/memory_extraction_status.dart';
import '../../state/connection_controller.dart';
import '../../state/memory_artifacts.dart';
import '../../state/memory_controller.dart';
import '../../state/memory_extraction.dart';
import '../../state/memory_context.dart';
import '../../state/memory_view.dart';
import '../lookup/id_lookup.dart';
import '../../widgets/empty_state.dart';
import '../settings/settings_page.dart';
import 'widgets/fact_card.dart';
import 'widgets/fact_row.dart';
import 'memory_artifacts_list.dart';

/// 整页的记忆浏览器 —— 这个应用的全部。
///
/// # 为什么它值得是一整页
///
/// 在 Cormex 里同样的东西是一条 320px 的边栏，只在对话页里够得着。而检索、
/// 出处、按系统时间回放这三样，恰好是这个项目唯一不可替代的部分（别家给的
/// 是一个相似度分数）。挤在边栏里等于把它们藏起来 ——「能看见自己的记忆」
/// 本身就该是一件事，不是聊天的附属品。
///
/// Cormex 里根本没有聊天，所以这里连那条边栏都不留。
class MemoryBrowser extends ConsumerStatefulWidget {
  const MemoryBrowser({super.key});

  @override
  ConsumerState<MemoryBrowser> createState() => _MemoryBrowserState();
}

class _MemoryBrowserState extends ConsumerState<MemoryBrowser> {
  MemoryArtifactKind? _kind;
  MemoryExtractionStatus? _lastExtraction;
  bool _factsStale = false;
  late final TextEditingController _query = TextEditingController(
    text: ref.read(memoryControllerProvider).query,
  );

  /// 时间轴的左端。**只往早走，不往晚回。**
  ///
  /// 它取自当前结果里最早那条的 `created_at`。而结果会随着拖动而变 ——
  /// 如果跟着一起缩，滑块会在拖的过程中自己改变量程，手感是「越拖越跑」。
  /// 单调放宽让量程稳下来，代价只是回到现在之后左端仍停在那个更早的日期，
  /// 而那恰好是对的：那条记忆确实存在过。
  DateTime? _earliest;

  /// 拖动过程中的位置。松手才真的去检索。
  double? _dragging;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted) return;
      ref.read(memoryArtifactsProvider.notifier).select(null);
      if (!ref.read(memoryControllerProvider).hasSearched) {
        ref.read(memoryControllerProvider.notifier).search();
      }
    });
  }

  @override
  void dispose() {
    _query.dispose();
    super.dispose();
  }

  /// 从结果里把时间轴左端放宽。
  void _widen(MemoryState state) {
    final stamps = state.result.facts
        .map((f) => f.createdAt)
        .whereType<DateTime>();
    if (stamps.isEmpty) return;
    final oldest = stamps.reduce((a, b) => a.isBefore(b) ? a : b);
    if (_earliest == null || oldest.isBefore(_earliest!)) {
      // build 里改状态要排到帧后，否则是「build 期间 setState」
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) setState(() => _earliest = oldest);
      });
    }
  }

  void _selectKind(MemoryArtifactKind? kind) {
    if (_kind == kind) return;
    ref.read(memoryArtifactsProvider.notifier).select(kind);
    setState(() => _kind = kind);
    if (kind == null &&
        (_factsStale || !ref.read(memoryControllerProvider).hasSearched)) {
      _factsStale = false;
      ref.read(memoryControllerProvider.notifier).search();
    }
  }

  void _refresh() {
    final kind = _kind;
    if (kind == null) {
      ref.read(memoryControllerProvider.notifier).search();
    } else {
      ref.read(memoryArtifactsProvider.notifier).load(kind, refresh: true);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final state = ref.watch(memoryControllerProvider);
    final notifier = ref.read(memoryControllerProvider.notifier);
    final artifacts = ref.watch(memoryArtifactsProvider)[_kind];
    final loading = _kind == null ? state.loading : artifacts?.loading == true;
    final loaded = _kind == null
        ? state.hasSearched
        : artifacts?.hasLoaded == true;
    final count = _kind == null
        ? state.result.facts.length
        : artifacts?.items.length ?? 0;
    ref.listen(memoryExtractionProvider, (previous, next) {
      final before = _lastExtraction;
      final after = next.asData?.value;
      if (after == null) return;
      _lastExtraction = after;
      if (before == null) return;
      if ((
            before.memoryCount,
            before.summaryCount,
            before.factsCreated,
            before.processedChats,
            before.trial?['status'],
            before.trial?['processed_messages'],
            before.trial?['memory_count'],
            before.trial?['summary_count'],
          ) !=
          (
            after.memoryCount,
            after.summaryCount,
            after.factsCreated,
            after.processedChats,
            after.trial?['status'],
            after.trial?['processed_messages'],
            after.trial?['memory_count'],
            after.trial?['summary_count'],
          )) {
        ref.read(memoryArtifactsProvider.notifier).invalidate();
        if (_kind == null) {
          notifier.search();
        } else {
          _factsStale = true;
        }
      }
    });
    _widen(state);

    return Scaffold(
      appBar: AppBar(
        titleSpacing: 20,
        // 宽屏时品牌标识已经在侧栏顶上，页头只说这是哪一页
        title: MediaQuery.sizeOf(context).width >= HomePage.wideBreakpoint
            ? const Text('我的记忆')
            : Row(
                children: [
                  Icon(
                    Icons.psychology_rounded,
                    size: 20,
                    color: scheme.secondary,
                  ),
                  const SizedBox(width: 9),
                  const Flexible(
                    child: Text('Cormex 记忆', overflow: TextOverflow.ellipsis),
                  ),
                ],
              ),
        actions: [
          if (_kind == null) const _ViewToggle(),
          const IdLookupButton(),
          IconButton(
            tooltip: '设置',
            icon: const Icon(Icons.settings_outlined),
            onPressed: () => Navigator.of(context).push(
              MaterialPageRoute<void>(builder: (_) => const SettingsPage()),
            ),
          ),
          // 检索到几条是这一页最要紧的一个数字：它同时回答「有没有」与
          // 「回放到那一刻少了多少」
          if (loaded && !loading)
            Padding(
              padding: const EdgeInsets.only(right: 4),
              child: Center(
                child: Text(
                  '$count 条',
                  style: theme.textTheme.labelMedium?.copyWith(
                    color: scheme.onSurfaceVariant,
                  ),
                ),
              ),
            ),
          IconButton(
            onPressed: loading ? null : _refresh,
            iconSize: 18,
            tooltip: _kind == null ? '重新检索' : '刷新当前类别',
            icon: const Icon(Icons.refresh_rounded),
          ),
          const _ConnectionMenu(),
          const SizedBox(width: 8),
        ],
      ),
      body: CustomScrollView(
        slivers: [
          SliverToBoxAdapter(
            child: Column(
              children: [
                _MemoryCategories(kind: _kind, onSelected: _selectKind),
                if (_kind == null) ...[
                  Padding(
                    padding: const EdgeInsets.fromLTRB(20, 14, 20, 8),
                    child: Center(
                      child: ConstrainedBox(
                        constraints: const BoxConstraints(maxWidth: 900),
                        child: TextField(
                          controller: _query,
                          autofocus: true,
                          onChanged: notifier.setQuery,
                          onSubmitted: (_) => notifier.search(),
                          textInputAction: TextInputAction.search,
                          decoration: InputDecoration(
                            hintText: '检索记忆，留空浏览最近记录',
                            prefixIcon: const Icon(
                              Icons.search_rounded,
                              size: 20,
                            ),
                            suffixIcon: state.query.isEmpty
                                ? null
                                : IconButton(
                                    icon: const Icon(
                                      Icons.close_rounded,
                                      size: 17,
                                    ),
                                    onPressed: () {
                                      _query.clear();
                                      notifier.setQuery('');
                                      notifier.search();
                                    },
                                  ),
                          ),
                        ),
                      ),
                    ),
                  ),
                  if (_earliest != null || state.isTimeTravelling)
                    _Timeline(
                      earliest: _earliest,
                      state: state,
                      dragging: _dragging,
                      onDrag: (v) => setState(() => _dragging = v),
                      onSettled: (at) {
                        setState(() => _dragging = null);
                        notifier.setAsOf(at);
                      },
                    ),
                ],
                const Divider(height: 1),
              ],
            ),
          ),
          SliverFillRemaining(
            child: _kind == null
                ? _Grid(state: state)
                : MemoryArtifactsList(kind: _kind!),
          ),
        ],
      ),
    );
  }
}

class _MemoryCategories extends StatelessWidget {
  const _MemoryCategories({required this.kind, required this.onSelected});
  final MemoryArtifactKind? kind;
  final ValueChanged<MemoryArtifactKind?> onSelected;

  @override
  Widget build(BuildContext context) => Center(
    child: ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 1100),
      child: Padding(
        padding: const EdgeInsets.fromLTRB(20, 8, 20, 8),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: Row(
                children: [
                  ChoiceChip(
                    key: const ValueKey('memory-category-memory'),
                    label: const Text('记忆'),
                    selected: kind == null,
                    onSelected: (_) => onSelected(null),
                  ),
                  for (final item in MemoryArtifactKind.values) ...[
                    const SizedBox(width: 8),
                    ChoiceChip(
                      key: ValueKey('memory-category-${item.name}'),
                      label: Text(item.label),
                      selected: kind == item,
                      onSelected: (_) => onSelected(item),
                    ),
                  ],
                ],
              ),
            ),
            const SizedBox(height: 8),
            Text(
              kind?.description ?? '从完整问答中自动提取、校验和更新可复用信息，可随时查看保留依据与来源。',
              style: Theme.of(context).textTheme.bodySmall,
            ),
          ],
        ),
      ),
    ),
  );
}

/// 能拖的时间轴 —— 这个产品对标 mem0 / zep / Letta 时唯一的差异点。
///
/// # 为什么日期选择器不够
///
/// 双时间轴回放的说服力全在**连续性**上：拖着滑块往回走，看着一条条事实
/// 消失、剩下的那些就是你当时真正知道的。选个日期再点确定，同一件事变成了
/// 一次查询 —— 结果一样，但没人会「看见」它。
///
/// # 松手才检索
///
/// 每帧发一次请求既打后端，也让结果在手指底下不停跳。拖的时候只改上面那行
/// 日期，松手才真的去查 —— 那一下的「刷新」正好是想要的那个瞬间。
class _Timeline extends StatelessWidget {
  const _Timeline({
    required this.earliest,
    required this.state,
    required this.dragging,
    required this.onDrag,
    required this.onSettled,
  });

  final DateTime? earliest;
  final MemoryState state;
  final double? dragging;
  final ValueChanged<double> onDrag;
  final ValueChanged<DateTime?> onSettled;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final now = DateTime.now();
    final start = earliest;

    // 还没有结果、或者所有记忆都在同一刻 —— 没有可拖的区间。画一条禁用的
    // 轨道而不是整个不画：位置固定，用户不会觉得它「有时候才有」。
    final span = start == null ? Duration.zero : now.difference(start);
    final enabled = span.inMinutes > 0;

    final position =
        dragging ??
        (state.asOf == null || !enabled
            ? 1.0
            : (state.asOf!.difference(start!).inMinutes / span.inMinutes).clamp(
                0.0,
                1.0,
              ));
    final at = !enabled || position >= 1.0
        ? null
        : start!.add(Duration(minutes: (span.inMinutes * position).round()));

    final invalidated = state.result.invalidatedCount;

    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 900),
        child: Padding(
          padding: const EdgeInsets.fromLTRB(20, 0, 20, 6),
          child: Column(
            children: [
              Row(
                children: [
                  Icon(
                    at == null
                        ? Icons.schedule_rounded
                        : Icons.history_toggle_off_rounded,
                    size: 16,
                    color: at == null
                        ? scheme.onSurfaceVariant
                        : scheme.secondary,
                  ),
                  const SizedBox(width: 8),
                  SizedBox(
                    width: 168,
                    child: Text(
                      at == null ? '当前时刻的记忆' : '回放至 ${formatDate(at)}',
                      style: theme.textTheme.labelMedium?.copyWith(
                        color: at == null
                            ? scheme.onSurfaceVariant
                            : scheme.secondary,
                        fontWeight: at == null ? null : FontWeight.w600,
                      ),
                    ),
                  ),
                  Expanded(
                    child: Slider(
                      value: position,
                      onChanged: enabled ? onDrag : null,
                      onChangeEnd: enabled
                          ? (v) => onSettled(
                              v >= 1.0
                                  ? null
                                  : start!.add(
                                      Duration(
                                        minutes: (span.inMinutes * v).round(),
                                      ),
                                    ),
                            )
                          : null,
                    ),
                  ),
                  if (at != null)
                    TextButton(
                      onPressed: () => onSettled(null),
                      child: const Text('回到现在'),
                    ),
                ],
              ),
              // 回放最值钱的那一句话：这一屏里有几条是「当时我以为是真的、
              // 后来被推翻了」。不带 as_of 的检索恒为 0（那条路查的是
              // active_facts 视图），所以这一行只在真的回放到过去时才出现 ——
              // 而它出现的时候，就是这个产品与「一个相似度分数」的分界线。
              if (invalidated > 0)
                Padding(
                  padding: const EdgeInsets.only(left: 24, bottom: 4),
                  child: Row(
                    children: [
                      Icon(
                        Icons.history_rounded,
                        size: 13,
                        color: scheme.error,
                      ),
                      const SizedBox(width: 6),
                      Expanded(
                        child: Text(
                          '其中 $invalidated 条在那时成立，此后已被推翻 —— '
                          '它们照样列出来，因为那正是当时的真实认知。',
                          style: theme.textTheme.labelSmall?.copyWith(
                            color: scheme.error,
                          ),
                        ),
                      ),
                    ],
                  ),
                ),
            ],
          ),
        ),
      ),
    );
  }
}

/// 宽屏下多列铺开。
class _Grid extends ConsumerWidget {
  const _Grid({required this.state});

  final MemoryState state;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    if (state.loading && state.result.isEmpty) {
      return const Center(
        child: SizedBox(
          width: 22,
          height: 22,
          child: CircularProgressIndicator(strokeWidth: 2.2),
        ),
      );
    }
    if (state.error != null) {
      return EmptyState(
        icon: Icons.cloud_off_rounded,
        title: '检索失败',
        description: state.error,
        tone: EmptyStateTone.error,
      );
    }
    if (state.result.isEmpty) {
      final extraction = ref.watch(memoryExtractionProvider).asData?.value;
      final explanation = extraction?.hasAnalyzedChats == true
          ? '已分析聊天，当前没有可用记忆。可以查看会话摘要和来源了解分析内容。'
          : switch (extraction?.phase) {
              'trial_running' => '正在分析选定样本，自动保留的可复用信息会显示在这里；全量提取未开启。',
              'paused' => '聊天已采集保存，但记忆提取尚未开启。可在设置中先选择小样本分析。',
              'needs_configuration' => '请在设置中完善模型配置，再选择小样本分析。',
              'error' => '提取遇到问题，处理位置已保留。请检查上方状态并重试。',
              'processing' || 'queued' => '正在分批分析聊天，自动保留可复用信息并更新会话摘要。',
              'caught_up' => '聊天分析已完成，当前没有可用记忆。可以查看会话摘要。',
              _ => '记忆来自聊天中的可复用信息，可在设置中查看模型配置与小样本分析。',
            };
      return EmptyState(
        icon: Icons.psychology_outlined,
        title: state.query.isEmpty ? '暂无记忆' : '没有相关的记忆',
        description: state.isTimeTravelling
            ? '在 ${formatDate(state.asOf)} 这个时点，Cormex 还不知道相关的事。'
            : state.query.isEmpty
            ? explanation
            : '没有匹配的记忆。可以调整关键词，或查看会话摘要。',
      );
    }

    final facts = state.result.facts;
    final contexts = ref
        .watch(memoryContextsProvider(facts.map((f) => f.id).join(',')))
        .asData
        ?.value;
    if (ref.watch(memoryViewProvider) == MemoryViewMode.list) {
      return Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 900),
          child: ListView.separated(
            key: const ValueKey('memory-list-view'),
            primary: false,
            padding: const EdgeInsets.fromLTRB(20, 8, 20, 28),
            itemCount: facts.length,
            separatorBuilder: (_, _) => const Divider(height: 1),
            itemBuilder: (context, i) =>
                FactRow(fact: facts[i], contextInfo: contexts?[facts[i].id]),
          ),
        ),
      );
    }
    return LayoutBuilder(
      builder: (context, box) {
        // 一列 420px 上下。再宽就让列变宽而不是加列 —— 一条事实是一句话，
        // 排成太窄的一条会被折成四五行。
        final columns = (box.maxWidth / 420).floor().clamp(1, 3);
        return Center(
          child: ConstrainedBox(
            constraints: BoxConstraints(maxWidth: columns * 460),
            child: GridView.builder(
              primary: false,
              padding: const EdgeInsets.fromLTRB(20, 14, 20, 28),
              gridDelegate: SliverGridDelegateWithFixedCrossAxisCount(
                crossAxisCount: columns,
                crossAxisSpacing: 12,
                mainAxisSpacing: 12,
                // 卡片高度不定（陈述长短不一），给一个偏矮的固定高度让内容
                // 自己滚 —— 定得太高会在短句上留大片空白，定死又会把长句截掉。
                mainAxisExtent: 270,
              ),
              itemCount: facts.length,
              itemBuilder: (context, i) => SingleChildScrollView(
                child: FactCard(
                  fact: facts[i],
                  contextInfo: contexts?[facts[i].id],
                  channels: state.result.channels[facts[i].id],
                ),
              ),
            ),
          ),
        );
      },
    );
  }
}

/// 卡片 / 列表切换。
class _ViewToggle extends ConsumerWidget {
  const _ViewToggle();

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final mode = ref.watch(memoryViewProvider);
    final cards = mode == MemoryViewMode.cards;
    return IconButton(
      key: const ValueKey('memory-view-toggle'),
      tooltip: cards ? '切换为列表' : '切换为卡片',
      icon: Icon(cards ? Icons.view_list_rounded : Icons.grid_view_rounded),
      onPressed: () => ref
          .read(memoryViewProvider.notifier)
          .set(cards ? MemoryViewMode.list : MemoryViewMode.cards),
    );
  }
}

/// 顶栏右侧：连到哪儿、以什么身份、怎么断开。
///
/// 挤在一个菜单里而不是摊在顶栏上：这些是「一天看一次」的信息，而顶栏的
/// 宽度该留给检索。
class _ConnectionMenu extends ConsumerWidget {
  const _ConnectionMenu();

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final conn = ref.watch(connectionProvider);
    final account = ref.watch(accountProvider).value;
    final health = conn.health;
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;

    // 认证关掉了要**明说**。cormexd 只有在有人特地写了 CORMEX_AUTH=disabled
    // 时才会到这个状态，而它每次启动都打一行警告。客户端什么都不说，就成了
    // 整个系统里唯一装作没看见的那一环。
    final warning = health?.authDisabled == true
        ? '认证已关闭：谁连上这个地址谁就能读全部记忆'
        : health?.databaseNote;

    return PopupMenuButton<String>(
      tooltip: '连接',
      icon: Badge(
        isLabelVisible: warning != null,
        backgroundColor: scheme.error,
        smallSize: 7,
        child: const Icon(Icons.account_circle_outlined, size: 20),
      ),
      onSelected: (v) {
        if (v == 'disconnect') {
          ref.read(connectionProvider.notifier).disconnect();
        }
      },
      itemBuilder: (context) => [
        PopupMenuItem<String>(
          enabled: false,
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                account?.username ?? '自托管（预共享凭据）',
                style: theme.textTheme.titleSmall?.copyWith(
                  color: scheme.onSurface,
                ),
              ),
              const SizedBox(height: 3),
              Text(
                conn.baseUrl.isEmpty ? '同源' : conn.baseUrl,
                style: theme.textTheme.labelSmall,
              ),
              if (health != null)
                Text(
                  'cormexd ${health.version}',
                  style: theme.textTheme.labelSmall,
                ),
              if (warning != null) ...[
                const SizedBox(height: 5),
                ConstrainedBox(
                  constraints: const BoxConstraints(maxWidth: 260),
                  child: Text(
                    warning,
                    style: theme.textTheme.labelSmall?.copyWith(
                      color: scheme.error,
                    ),
                  ),
                ),
              ],
            ],
          ),
        ),
        const PopupMenuDivider(),
        const PopupMenuItem<String>(
          value: 'disconnect',
          child: Row(
            children: [
              Icon(Icons.logout_rounded, size: 16),
              SizedBox(width: 10),
              Text('断开'),
            ],
          ),
        ),
      ],
    );
  }
}
