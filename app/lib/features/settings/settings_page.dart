import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../state/reading_preferences.dart';
import 'memory_extraction_settings_page.dart';

class SettingsPage extends ConsumerWidget {
  const SettingsPage({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final width = ref.watch(readingPreferencesProvider);
    return Scaffold(
      appBar: AppBar(title: const Text('设置')),
      body: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 640),
          child: ListView(
            padding: const EdgeInsets.all(24),
            children: [
              ListTile(
                contentPadding: EdgeInsets.zero,
                leading: const Icon(Icons.psychology_outlined),
                title: const Text('模型配置'),
                subtitle: const Text('记忆提取模型、连接测试和小样本验证'),
                trailing: const Icon(Icons.chevron_right),
                onTap: () => Navigator.of(context).push(
                  MaterialPageRoute<void>(
                    builder: (_) => const MemoryExtractionSettingsPage(),
                  ),
                ),
              ),
              const Divider(height: 32),
              Text('阅读', style: Theme.of(context).textTheme.titleLarge),
              const SizedBox(height: 16),
              Text(
                '聊天正文宽度：${width.round()} px',
                style: Theme.of(context).textTheme.titleMedium,
              ),
              const SizedBox(height: 8),
              const Text('较窄适合阅读长文本；屏幕较小时会自动适配。'),
              Slider(
                value: width,
                min: 560,
                max: 1400,
                divisions: 42,
                label: '${width.round()} px',
                onChanged: (value) => ref
                    .read(readingPreferencesProvider.notifier)
                    .preview(value),
                onChangeEnd: (value) async {
                  final saved = await ref
                      .read(readingPreferencesProvider.notifier)
                      .setWidth(value);
                  if (!saved && context.mounted) {
                    ScaffoldMessenger.of(context).showSnackBar(
                      const SnackBar(content: Text('当前浏览器无法保存设置，本次调整仍然生效。')),
                    );
                  }
                },
              ),
            ],
          ),
        ),
      ),
    );
  }
}
