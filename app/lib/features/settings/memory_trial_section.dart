import 'dart:async';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../../api/api_exception.dart';
import '../../models/memory_extraction_status.dart';
import '../../state/connection_controller.dart';
import '../../state/memory_extraction.dart';
import '../sessions/session_detail_page.dart';
import '../../models/session_memory.dart';

final memoryDependenciesProvider = FutureProvider<Map<String, dynamic>>(
  (ref) => ref.watch(apiProvider).memoryExtractionDependencies(),
);

class MemoryTrialSection extends ConsumerStatefulWidget {
  const MemoryTrialSection({
    super.key,
    required this.status,
    this.unsaved = false,
  });
  final MemoryExtractionStatus status;
  final bool unsaved;
  @override
  ConsumerState<MemoryTrialSection> createState() => _MemoryTrialSectionState();
}

class _MemoryTrialSectionState extends ConsumerState<MemoryTrialSection> {
  Timer? _timer;
  bool _busy = false;
  String? _error;
  @override
  void initState() {
    super.initState();
    _timer = Timer.periodic(const Duration(seconds: 3), (_) {
      if (mounted &&
          widget.status.trialRunning &&
          (ModalRoute.of(context)?.isCurrent ?? true)) {
        ref.invalidate(memoryExtractionProvider);
      }
    });
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  Future<void> _choose() async {
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final candidates = await ref.read(apiProvider).memoryTrialCandidates();
      if (!mounted) return;
      final selected = <String>{};
      final ids = await showDialog<List<String>>(
        context: context,
        builder: (context) => StatefulBuilder(
          builder: (context, setLocal) => AlertDialog(
            title: const Text('选择少量短聊天'),
            content: SizedBox(
              width: 540,
              height: 360,
              child: candidates.isEmpty
                  ? const Text('暂时没有合适的短聊天。')
                  : ListView(
                      children: [
                        const Text(
                          '建议先选择 2 个旧聊天验证。最多 3 个聊天、合计 8 条对话消息；按完整问答结合有限上下文分析。',
                        ),
                        const SizedBox(height: 12),
                        for (final c in candidates)
                          CheckboxListTile(
                            contentPadding: EdgeInsets.zero,
                            title: Text(
                              c['title'] as String? ?? '未命名聊天',
                              maxLines: 2,
                              overflow: TextOverflow.ellipsis,
                            ),
                            subtitle: Text(
                              '${c['agent']} · ${c['message_count']} 条消息',
                            ),
                            value: selected.contains(c['id']),
                            onChanged: (v) => setLocal(() {
                              if (v == true) {
                                if (selected.length < 3) {
                                  selected.add(c['id'] as String);
                                }
                              } else {
                                selected.remove(c['id']);
                              }
                            }),
                          ),
                      ],
                    ),
            ),
            actions: [
              TextButton(
                onPressed: () => Navigator.pop(context),
                child: const Text('取消'),
              ),
              FilledButton(
                onPressed: selected.isEmpty
                    ? null
                    : () => Navigator.pop(context, selected.toList()),
                child: Text('仅提取选中的 ${selected.length} 个聊天'),
              ),
            ],
          ),
        ),
      );
      if (ids != null && mounted) {
        await ref.read(apiProvider).startMemoryTrial(ids);
        ref.invalidate(memoryExtractionProvider);
      }
    } on CormexApiException catch (e) {
      if (mounted) setState(() => _error = e.message);
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _cancel() async {
    setState(() => _busy = true);
    try {
      await ref.read(apiProvider).cancelMemoryTrial();
      ref.invalidate(memoryExtractionProvider);
    } on CormexApiException catch (e) {
      if (mounted) setState(() => _error = e.message);
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final s = widget.status;
    final j = s.trial;
    final theme = Theme.of(context);
    final selections = (j?['selections'] as List? ?? [])
        .whereType<Map>()
        .toList();
    final processedMessages = (j?['processed_messages'] as num?)?.toInt() ?? 0;
    final totalMessages = (j?['total_messages'] as num?)?.toInt() ?? 0;
    final label = switch (j?['status']) {
      'running' => '正在分析',
      'completed' => '分析完成',
      'failed' => '分析未完成',
      'cancelled' => '已停止',
      _ => '尚未开始',
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const Divider(height: 36),
        Text('小样本验证', style: theme.textTheme.titleLarge),
        const SizedBox(height: 8),
        const Text(
          '先用 2 个旧聊天验证。按完整问答结合有限上下文分析，完成后自动停止；全量提取保持关闭。每次最多 3 个聊天、8 条对话消息、24000 字符。',
        ),
        const SizedBox(height: 8),
        const Text(
          '系统自动判断信息是否可复用，并结合对话与工具证据提取、校验和更新记忆。会话摘要独立保存；没有可保留的记忆时可返回 0 条。',
        ),
        const SizedBox(height: 12),
        ref
            .watch(memoryDependenciesProvider)
            .when(
              loading: () => const Text('正在检查向量服务…'),
              error: (_, _) => TextButton(
                onPressed: () => ref.invalidate(memoryDependenciesProvider),
                child: const Text('向量服务状态读取失败，点击重试'),
              ),
              data: (d) => Row(
                children: [
                  Expanded(
                    child: Text(
                      '向量服务（服务端）：${d['embedding_model'] ?? '未知'} · ${d['embedding_ready'] == true ? '已就绪' : '未就绪'}',
                    ),
                  ),
                  IconButton(
                    tooltip: '检查向量服务',
                    onPressed: () => ref.invalidate(memoryDependenciesProvider),
                    icon: const Icon(Icons.refresh, size: 18),
                  ),
                ],
              ),
            ),
        if (j != null) ...[
          const SizedBox(height: 14),
          Text(label, style: theme.textTheme.titleMedium),
          Text(
            isSessionJob(j)
                ? '单会话生成：已分析 $processedMessages / $totalMessages 轮问答'
                : '已检查 ${j['processed_chats'] ?? 0} / ${selections.length} 个聊天 · 已处理 $processedMessages / $totalMessages 条消息',
          ),
          const SizedBox(height: 8),
          Wrap(
            spacing: 16,
            runSpacing: 8,
            children: [
              for (final label in s.trialOutputCountLabels) Text(label),
            ],
          ),
          if (s.trialCompletedWithNoMemories) ...[
            const SizedBox(height: 8),
            const Text('本次保留 0 条记忆。会话摘要和来源可用于了解本次分析结果。'),
          ],
          const SizedBox(height: 8),
          LinearProgressIndicator(
            value: totalMessages > 0
                ? (processedMessages / totalMessages).clamp(0, 1).toDouble()
                : j['status'] == 'completed'
                ? 1
                : 0,
          ),
          for (final c in selections)
            if (c['session_id'] case final String sessionId
                when sessionId.isNotEmpty)
              TextButton.icon(
                onPressed: () => Navigator.of(context).push(
                  MaterialPageRoute<void>(
                    builder: (_) => SessionDetailPage(sessionId: sessionId),
                  ),
                ),
                icon: const Icon(Icons.chat_bubble_outline, size: 16),
                label: Text(
                  c['title'] as String? ?? '查看聊天',
                  maxLines: 2,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
          if (j['last_error'] case final String message)
            Text(message, style: TextStyle(color: theme.colorScheme.error)),
        ],
        const SizedBox(height: 14),
        Wrap(
          spacing: 12,
          runSpacing: 8,
          children: [
            FilledButton.tonalIcon(
              onPressed:
                  _busy ||
                      s.trialRunning ||
                      s.enabled ||
                      widget.unsaved ||
                      !s.modelConfigured
                  ? null
                  : _choose,
              icon: const Icon(Icons.science_outlined),
              label: Text(_busy ? '处理中…' : '选择聊天试提取'),
            ),
            if (s.trialRunning)
              OutlinedButton(
                onPressed: _busy ? null : _cancel,
                child: const Text('停止验证'),
              ),
          ],
        ),
        if (s.enabled) const Text('请先关闭全量自动提取，再启动小样本验证。'),
        if (widget.unsaved) const Text('模型配置有修改，请先保存。'),
        if (_error != null)
          Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
      ],
    );
  }
}
