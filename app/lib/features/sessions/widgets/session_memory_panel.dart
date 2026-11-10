import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/api_exception.dart';
import '../../../core/formatting.dart';
import '../../../core/theme.dart';
import '../../../models/memory_fact.dart';
import '../../../models/session_memory.dart';
import '../../../state/connection_controller.dart';
import '../../../state/memory_context.dart';
import '../../../widgets/id_chip.dart';
import '../../memory/widgets/fact_row.dart';
import '../../memory/widgets/memory_reading.dart';

/// 会话页标题下的一行：这个会话的 ID、它生成了几条记忆、为它生成记忆。
///
/// # 为什么生成要先确认
///
/// 一次点击会把最多 [SessionMemoryStatus.maxTurnsPerJob] 轮对话发给模型，
/// 花的是真钱。确认框写明这次会分析多少轮，而不是让一个按钮静默扣费。
///
/// # 为什么一次有上限
///
/// 编程会话常有几百轮。上限让一次点击的花费可预期；没分析完的部分，再点
/// 一次「继续生成」从上次停下的地方接着来（服务端按同一个游标续）。
class SessionMemoryPanel extends ConsumerStatefulWidget {
  const SessionMemoryPanel({super.key, required this.sessionId});
  final String sessionId;

  @override
  ConsumerState<SessionMemoryPanel> createState() => _SessionMemoryPanelState();
}

class _SessionMemoryPanelState extends ConsumerState<SessionMemoryPanel> {
  SessionMemoryStatus? _status;
  String? _error;
  bool _starting = false;
  Timer? _poll;

  @override
  void initState() {
    super.initState();
    _refresh();
  }

  @override
  void dispose() {
    _poll?.cancel();
    super.dispose();
  }

  Future<void> _refresh() async {
    try {
      final status = await ref
          .read(apiProvider)
          .sessionMemoryStatus(widget.sessionId);
      if (!mounted) return;
      final finished = (_status?.running ?? false) && !status.running;
      setState(() {
        _status = status;
        _error = null;
      });
      _poll?.cancel();
      if (status.running) {
        _poll = Timer(const Duration(seconds: 2), _refresh);
      }
      if (finished && mounted) {
        final job = status.job;
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(
            content: Text(
              job?.status == 'completed'
                  ? '生成完成：分析了 ${job!.processed} 轮，本会话现有 ${status.memoryCount} 条记忆'
                  : job?.error ?? '生成已结束',
            ),
          ),
        );
      }
    } on CormexApiException catch (e) {
      if (mounted) setState(() => _error = e.message);
    }
  }

  Future<void> _start() async {
    final status = _status;
    if (status == null) return;
    // A redo resets the cursor: the server has not counted those turns yet,
    // so the honest number to show is the per-job ceiling.
    final turns = status.canRedo
        ? status.maxTurnsPerJob
        : status.pendingTurns < status.maxTurnsPerJob
        ? status.pendingTurns
        : status.maxTurnsPerJob;
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(status.canRedo ? '重新分析这个会话' : '为这个会话生成记忆'),
        content: Text(
          status.canRedo
              ? '这个会话之前已分析完，但没有提炼出任何记忆。将重置分析进度并重新分析整个会话，'
                  '这次最多 $turns 轮。\n\n会产生模型费用。'
              : '将用设置页配置的模型分析这个会话里还没分析过的对话：'
                  '这次最多 $turns 轮'
                  '${status.pendingTurns > status.maxTurnsPerJob ? '（约还有 ${status.pendingTurns} 轮未分析，剩下的可以再点「继续生成」）' : ''}。\n\n'
                  '会产生模型费用。已经分析过的部分不会重复计费。',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('取消'),
          ),
          FilledButton(
            key: const ValueKey('session-memory-confirm'),
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('开始生成'),
          ),
        ],
      ),
    );
    if (confirmed != true || !mounted) return;
    setState(() => _starting = true);
    try {
      await ref
          .read(apiProvider)
          .startSessionMemory(widget.sessionId, redo: status.canRedo);
      await _refresh();
    } on CormexApiException catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(
          context,
        ).showSnackBar(SnackBar(content: Text(e.message)));
      }
    } finally {
      if (mounted) setState(() => _starting = false);
    }
  }

  Future<void> _stop() async {
    try {
      await ref.read(apiProvider).cancelMemoryTrial();
    } on CormexApiException catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(
          context,
        ).showSnackBar(SnackBar(content: Text(e.message)));
      }
    }
    await _refresh();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final status = _status;
    final job = status?.job;
    final running = status?.running ?? false;

    Widget action;
    if (status == null) {
      action = const SizedBox.shrink();
    } else if (!status.supported) {
      action = Text('这个来源暂不支持生成记忆', style: theme.textTheme.labelSmall);
    } else if (running) {
      action = Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          const SizedBox(
            width: 14,
            height: 14,
            child: CircularProgressIndicator(strokeWidth: 2),
          ),
          const SizedBox(width: 8),
          Text(
            '正在生成 ${job!.processed}/${job.total} 轮',
            style: theme.textTheme.labelMedium,
          ),
          TextButton(onPressed: _stop, child: const Text('停止')),
        ],
      );
    } else if (status.pendingTurns == 0 && !status.canRedo) {
      action = Text(
        '已全部分析',
        key: const ValueKey('session-memory-caught-up'),
        style: theme.textTheme.labelMedium?.copyWith(
          color: scheme.onSurfaceVariant,
        ),
      );
    } else {
      final resume = status.memoryCount > 0 || job != null;
      action = Tooltip(
        message: status.otherJobRunning ? '已有另一个记忆生成任务在运行' : '',
        child: FilledButton.tonalIcon(
          key: const ValueKey('session-memory-generate'),
          onPressed: _starting || status.otherJobRunning ? null : _start,
          icon: Icon(
            status.canRedo ? Icons.refresh_rounded : Icons.auto_awesome_outlined,
            size: 16,
          ),
          label: Text(
            status.canRedo ? '重新生成记忆' : resume ? '继续生成记忆' : '生成记忆',
          ),
        ),
      );
    }

    final s = status;
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 0, 16, 12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          if (s?.summary case final String digest when digest.trim().isNotEmpty)
            _Digest(title: s?.summaryTitle, text: digest),
          Wrap(
        crossAxisAlignment: WrapCrossAlignment.center,
        spacing: 12,
        runSpacing: 8,
        children: [
          IdChip(id: widget.sessionId, label: '会话'),
          if (status != null)
            ActionChip(
              key: const ValueKey('session-memory-count'),
              avatar: const Icon(Icons.bookmark_outline_rounded, size: 16),
              label: Text('本会话记忆 ${status.memoryCount} 条'),
              onPressed: status.memoryCount == 0
                  ? null
                  : () => showSessionMemories(
                      context,
                      widget.sessionId,
                      count: status.memoryCount,
                    ),
            ),
          action,
          if (_error != null)
            Text(
              _error!,
              style: theme.textTheme.labelSmall?.copyWith(color: scheme.error),
            ),
          if (!running && job?.status == 'failed' && job?.error != null)
            Text(
              job!.error!,
              style: theme.textTheme.labelSmall?.copyWith(color: scheme.error),
            ),
          ],
          ),
        ],
      ),
    );
  }
}

/// 会话要点：一段能独立读懂的精炼总结，排在 ID / 计数 / 按钮那一行之前。
/// 这是「这个会话到底说了什么」的答案，比一堆属性和零条事实更有用。
class _Digest extends StatelessWidget {
  const _Digest({this.title, required this.text});
  final String? title;
  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final accent = CormexTheme.memoryAccent(theme.brightness);
    return Container(
      margin: const EdgeInsets.only(bottom: CormexSpacing.md),
      padding: const EdgeInsets.all(CormexSpacing.md + 2),
      decoration: BoxDecoration(
        color: scheme.surfaceContainerLow,
        borderRadius: BorderRadius.circular(CormexRadii.card),
        border: Border.all(color: scheme.outlineVariant),
      ),
      child: IntrinsicHeight(
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Container(
              width: 3,
              decoration: BoxDecoration(
                color: accent,
                borderRadius: BorderRadius.circular(2),
              ),
            ),
            const SizedBox(width: CormexSpacing.md),
            Expanded(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisSize: MainAxisSize.min,
                children: [
                  Text(
                    title?.trim().isNotEmpty == true ? title! : '本会话要点',
                    style: theme.textTheme.titleSmall?.copyWith(
                      color: accent,
                      fontWeight: FontWeight.w600,
                    ),
                  ),
                  const SizedBox(height: CormexSpacing.sm - 2),
                  Text(
                    plainStatement(text),
                    style: theme.textTheme.bodyMedium?.copyWith(height: 1.6),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}

final _sessionMemoriesProvider = FutureProvider.autoDispose
    .family<List<MemoryFact>, String>(
      (ref, id) => ref.watch(apiProvider).sessionMemories(id),
    );

/// 这个会话生成的全部记忆：会话 → 记忆这一侧的映射。
Future<void> showSessionMemories(
  BuildContext context,
  String sessionId, {
  required int count,
}) => showMemoryReadingDialog(
  context,
  child: _SessionMemories(sessionId: sessionId, count: count),
);

class _SessionMemories extends ConsumerWidget {
  const _SessionMemories({required this.sessionId, required this.count});
  final String sessionId;
  final int count;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final theme = Theme.of(context);
    final facts = ref.watch(_sessionMemoriesProvider(sessionId));
    final ids = facts.asData?.value.map((f) => f.id).take(100).join(',') ?? '';
    final contexts = ref.watch(memoryContextsProvider(ids)).asData?.value;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(24, 16, 12, 8),
          child: Row(
            children: [
              Expanded(
                child: Text('本会话记忆（$count）', style: theme.textTheme.titleLarge),
              ),
              IconButton(
                tooltip: '关闭',
                onPressed: () => Navigator.of(context).pop(),
                icon: const Icon(Icons.close),
              ),
            ],
          ),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(24, 0, 24, 8),
          child: IdLine(label: '会话 ID', id: sessionId),
        ),
        const Divider(height: 1),
        Expanded(
          child: facts.when(
            loading: () => const Center(child: CircularProgressIndicator()),
            error: (e, _) => Center(child: Text('$e')),
            data: (items) => ListView.separated(
              padding: const EdgeInsets.fromLTRB(20, 8, 20, 24),
              itemCount: items.length,
              separatorBuilder: (_, _) => const Divider(height: 1),
              itemBuilder: (context, i) =>
                  FactRow(fact: items[i], contextInfo: contexts?[items[i].id]),
            ),
          ),
        ),
      ],
    );
  }
}
