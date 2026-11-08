import 'dart:async';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../../api/api_exception.dart';
import '../../models/memory_extraction_status.dart';
import '../../state/connection_controller.dart';
import '../../state/memory_extraction.dart';

class MemoryExtractionPanel extends ConsumerStatefulWidget {
  const MemoryExtractionPanel({super.key});
  @override
  ConsumerState<MemoryExtractionPanel> createState() =>
      _MemoryExtractionPanelState();
}

class _MemoryExtractionPanelState extends ConsumerState<MemoryExtractionPanel> {
  Timer? _poll;
  bool _saving = false;
  @override
  void initState() {
    super.initState();
    _poll = Timer.periodic(const Duration(seconds: 5), (_) {
      if (!mounted ||
          !TickerMode.valuesOf(context).enabled ||
          !(ModalRoute.of(context)?.isCurrent ?? true)) {
        return;
      }
      final s = ref.read(memoryExtractionProvider).asData?.value;
      if (s != null &&
          (s.trialRunning ||
              (s.enabled && (s.pendingChats > 0 || s.phase == 'processing')))) {
        ref.invalidate(memoryExtractionProvider);
      }
    });
  }

  @override
  void dispose() {
    _poll?.cancel();
    super.dispose();
  }

  Future<void> _save(MemoryExtractionStatus s, bool on) async {
    setState(() => _saving = true);
    try {
      await ref.read(apiProvider).saveMemoryExtraction(s.settings(enabled: on));
      ref.invalidate(memoryExtractionProvider);
    } on CormexApiException catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(
          context,
        ).showSnackBar(SnackBar(content: Text(e.message)));
      }
    } finally {
      if (mounted) setState(() => _saving = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 1100),
        child: Padding(
          padding: EdgeInsets.zero,
          child: ref
              .watch(memoryExtractionProvider)
              .when(
                loading: () => const LinearProgressIndicator(),
                error: (e, _) => Material(
                  color: scheme.errorContainer,
                  borderRadius: BorderRadius.circular(12),
                  child: Padding(
                    padding: const EdgeInsets.all(12),
                    child: Row(
                      children: [
                        const Expanded(child: Text('无法读取记忆提取状态，请检查连接后重试。')),
                        TextButton(
                          onPressed: () =>
                              ref.invalidate(memoryExtractionProvider),
                          child: const Text('重试'),
                        ),
                      ],
                    ),
                  ),
                ),
                data: (s) {
                  final color = s.phase == 'error'
                      ? scheme.error
                      : s.phase == 'needs_configuration'
                      ? Colors.orange.shade800
                      : s.enabled
                      ? scheme.primary
                      : scheme.onSurfaceVariant;
                  return DecoratedBox(
                    decoration: BoxDecoration(
                      color: scheme.surfaceContainerLow,
                      borderRadius: BorderRadius.circular(16),
                      border: Border.all(color: scheme.outlineVariant),
                    ),
                    child: Padding(
                      padding: const EdgeInsets.all(16),
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Row(
                            children: [
                              Icon(Icons.psychology_outlined, color: color),
                              const SizedBox(width: 10),
                              Expanded(
                                child: Wrap(
                                  crossAxisAlignment: WrapCrossAlignment.center,
                                  spacing: 12,
                                  runSpacing: 6,
                                  children: [
                                    Text(
                                      '记忆提取',
                                      style: theme.textTheme.titleMedium,
                                    ),
                                    Text(
                                      s.label,
                                      style: theme.textTheme.labelLarge
                                          ?.copyWith(color: color),
                                    ),
                                  ],
                                ),
                              ),
                              IconButton(
                                tooltip: '刷新提取状态',
                                onPressed: () =>
                                    ref.invalidate(memoryExtractionProvider),
                                icon: const Icon(
                                  Icons.refresh_rounded,
                                  size: 20,
                                ),
                              ),
                              Semantics(
                                label: '全量自动提取',
                                child: Switch.adaptive(
                                  value: s.enabled,
                                  onChanged: _saving
                                      ? null
                                      : (on) => _save(s, on),
                                ),
                              ),
                            ],
                          ),
                          Text(
                            s.trialRunning
                                ? '正在验证选定的短聊天；全量自动提取保持关闭。'
                                : s.enabled
                                ? '按完整问答和有限上下文自动提取、校验和更新记忆，并整理会话摘要。'
                                : s.hasAnalyzedChats || s.factsCreated > 0
                                ? '全量自动提取已关闭，已有记忆与会话摘要可继续阅读和追溯。'
                                : '聊天已保存。可先在下方「模型配置」里选 2 个旧聊天验证，不必开启全量提取。',
                            style: theme.textTheme.bodySmall,
                          ),
                          const SizedBox(height: 14),
                          Wrap(
                            spacing: 24,
                            runSpacing: 8,
                            children: [
                              Text(
                                '已检查 ${s.processedChats} / ${s.totalChats} 个聊天'
                                '${s.sinceLabel == null ? '' : '（${s.sinceLabel}）'}',
                              ),
                              Text('待处理 ${s.pendingChats} 个'),
                            ],
                          ),
                          const SizedBox(height: 8),
                          Wrap(
                            spacing: 20,
                            runSpacing: 6,
                            children: [
                              for (final label in s.outputCountLabels)
                                Text(label),
                            ],
                          ),
                          if (s.totalChats > 0) ...[
                            const SizedBox(height: 10),
                            LinearProgressIndicator(
                              value: (s.processedChats / s.totalChats).clamp(
                                0,
                                1,
                              ),
                              minHeight: 4,
                              borderRadius: BorderRadius.circular(4),
                            ),
                          ],
                          const SizedBox(height: 10),
                          if (s.trial case final trial?) ...[
                            Text(
                              '最近一次小样本${trial['status'] == 'completed'
                                  ? '分析完成'
                                  : trial['status'] == 'running'
                                  ? '提取中'
                                  : trial['status'] == 'cancelled'
                                  ? '已停止'
                                  : '未完成'} · ${trial['processed_chats'] ?? 0} / ${(trial['selections'] as List? ?? []).length} 个聊天 · ${trial['processed_messages'] ?? 0} / ${trial['total_messages'] ?? 0} 条消息',
                              style: theme.textTheme.bodySmall,
                            ),
                            Wrap(
                              spacing: 16,
                              runSpacing: 6,
                              children: [
                                for (final label in s.trialOutputCountLabels)
                                  Text(label, style: theme.textTheme.bodySmall),
                              ],
                            ),
                            if (s.trialCompletedWithNoMemories)
                              Text(
                                '本次保留 0 条记忆，可以查看会话摘要和来源了解分析结果。',
                                style: theme.textTheme.bodySmall,
                              ),
                            const SizedBox(height: 8),
                          ],
                          // The model is changed under "模型配置", right below
                          // this panel on the settings page
                          Text(
                            '${s.useServerModel ? '服务端默认' : '自定义模型'} · ${s.provider} / ${s.model}',
                            maxLines: 2,
                            overflow: TextOverflow.ellipsis,
                            style: theme.textTheme.bodySmall,
                          ),
                          if (s.lastError != null) ...[
                            const SizedBox(height: 6),
                            Text(
                              s.lastError!,
                              style: theme.textTheme.bodySmall?.copyWith(
                                color: scheme.error,
                              ),
                            ),
                            TextButton(
                              onPressed: _saving
                                  ? null
                                  : () => _save(s, s.enabled),
                              child: const Text('立即重试'),
                            ),
                          ],
                          if (!s.modelConfigured)
                            Text(
                              '当前模型不可用，请先完善模型配置。',
                              style: theme.textTheme.bodySmall?.copyWith(
                                color: color,
                              ),
                            ),
                        ],
                      ),
                    ),
                  );
                },
              ),
        ),
      ),
    );
  }
}
