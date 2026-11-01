import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../core/formatting.dart';
import '../../models/memory_artifact.dart';
import '../../state/memory_artifacts.dart';
import '../../widgets/empty_state.dart';
import 'widgets/artifact_sheet.dart';
import 'widgets/memory_reading.dart';

class MemoryArtifactsList extends ConsumerWidget {
  const MemoryArtifactsList({super.key, required this.kind});
  final MemoryArtifactKind kind;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final state =
        ref.watch(memoryArtifactsProvider)[kind] ??
        const MemoryArtifactsState();
    final notifier = ref.read(memoryArtifactsProvider.notifier);
    if (!state.hasLoaded && state.error == null && state.items.isEmpty) {
      return const Center(child: CircularProgressIndicator());
    }
    if (state.items.isEmpty) {
      return EmptyState(
        icon: state.error == null ? Icons.article_outlined : Icons.cloud_off,
        title: state.error == null ? '暂无${kind.label}' : '${kind.label}读取失败',
        description: state.error ?? '尚无会话摘要。小样本分析后，摘要会独立显示在这里。',
        tone: state.error == null
            ? EmptyStateTone.neutral
            : EmptyStateTone.error,
        action: state.error == null
            ? null
            : TextButton(
                onPressed: () => notifier.load(kind, refresh: true),
                child: const Text('重试'),
              ),
      );
    }
    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 1100),
        child: ListView.separated(
          primary: false,
          key: PageStorageKey('memory-${kind.name}'),
          padding: const EdgeInsets.fromLTRB(20, 14, 20, 28),
          itemCount: state.items.length + 1,
          separatorBuilder: (_, _) => const SizedBox(height: 12),
          itemBuilder: (context, i) {
            if (i < state.items.length) {
              return _ArtifactCard(artifact: state.items[i]);
            }
            return Column(
              children: [
                if (state.loading) const LinearProgressIndicator(),
                if (state.error case final error?) ...[
                  Text(
                    error,
                    style: TextStyle(
                      color: Theme.of(context).colorScheme.error,
                    ),
                  ),
                  TextButton(
                    onPressed: state.loading
                        ? null
                        : () => notifier.load(
                            kind,
                            more: state.failedMore,
                            refresh: !state.failedMore,
                          ),
                    child: Text(state.failedMore ? '重试加载更多' : '重试刷新'),
                  ),
                ] else if (state.nextCursor != null)
                  OutlinedButton.icon(
                    onPressed: state.loading
                        ? null
                        : () => notifier.load(kind, more: true),
                    icon: const Icon(Icons.expand_more),
                    label: const Text('加载更多'),
                  )
                else
                  Text('已显示全部 ${state.items.length} 条${kind.label}'),
              ],
            );
          },
        ),
      ),
    );
  }
}

class _ArtifactCard extends StatelessWidget {
  const _ArtifactCard({required this.artifact});
  final MemoryArtifact artifact;

  @override
  Widget build(BuildContext context) {
    final a = artifact;
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    return Material(
      color: scheme.surfaceContainerLow,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(14),
        side: BorderSide(color: scheme.outlineVariant),
      ),
      child: InkWell(
        borderRadius: BorderRadius.circular(14),
        onTap: () => showArtifactSheet(context, a),
        child: Padding(
          padding: const EdgeInsets.all(20),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(a.displayTitle, style: theme.textTheme.titleMedium),
              const SizedBox(height: 8),
              Wrap(
                spacing: 16,
                runSpacing: 6,
                children: [
                  Text(a.projectName ?? '未分类项目'),
                  Text(formatDate(a.createdAt)),
                ],
              ),
              const SizedBox(height: 12),
              ConstrainedBox(
                constraints: const BoxConstraints(maxHeight: 90),
                child: ClipRect(
                  child: IgnorePointer(
                    child: SingleChildScrollView(
                      physics: const NeverScrollableScrollPhysics(),
                      child: MemoryMarkdown(
                        String.fromCharCodes(a.markdown.runes.take(600)),
                      ),
                    ),
                  ),
                ),
              ),
              const SizedBox(height: 14),
              Text(
                '阅读摘要',
                style: theme.textTheme.labelLarge?.copyWith(
                  color: scheme.primary,
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
