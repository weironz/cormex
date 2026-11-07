import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../api/api_exception.dart';
import '../../models/archive_session.dart';
import '../../state/connection_controller.dart';

/// 选中的一份导出文件。
class PickedExport {
  const PickedExport({required this.name, required this.bytes});
  final String name;
  final List<int> bytes;
}

/// 打开系统的文件选择框。单独一个 provider，测试里换成直接返回一份文件。
final exportPickerProvider = Provider<Future<PickedExport?> Function()>(
  (ref) => () async {
    // If this throws "has not been implemented" in a browser build, the web
    // plugin was not registered: a build cache from before `file_picker` was
    // added. `flutter clean` (or deleting .dart_tool/flutter_build) fixes it
    final files = await FilePicker.pickFiles(
      dialogTitle: '选择聊天导出文件',
      type: FileType.custom,
      allowedExtensions: const ['zip', 'json'],
    );
    if (files.isEmpty) return null;
    final file = files.first;
    return PickedExport(name: file.name, bytes: await file.readAsBytes());
  },
);

enum _Step { choose, uploading, preview, running, done }

/// 把 ChatGPT / Claude / Grok / Gemini 的导出放进聊天档案。
///
/// 两步：先**预览**（新增、有变化、未变各多少，什么都不写），确认后再导入。
/// 同一份文件导第二次，全部是「未变」，什么都不写 —— 所以每月整包导一次
/// 是推荐用法，不必挑出新对话。
///
/// 网页版和 App 里的聊天存在厂商服务器上，Cormex 采不到，只能靠导出：
/// 这里说清楚去哪导，比让用户自己找要好。
class ImportArchiveDialog extends ConsumerStatefulWidget {
  const ImportArchiveDialog({super.key});

  @override
  ConsumerState<ImportArchiveDialog> createState() =>
      _ImportArchiveDialogState();
}

class _ImportArchiveDialogState extends ConsumerState<ImportArchiveDialog> {
  _Step _step = _Step.choose;
  String? _fileName;
  String? _handle;
  ArchiveImportSummary? _summary;
  String? _error;

  Future<void> _choose() async {
    setState(() => _error = null);
    final PickedExport? maybe;
    try {
      maybe = await ref.read(exportPickerProvider)();
    } on Object catch (e) {
      // A picker that fails must say so; an unhandled error here leaves the
      // dialog looking as if the button did nothing
      if (mounted) setState(() => _error = '打不开文件选择框：$e');
      return;
    }
    // A plain local, not the try-assigned one: the closures below would not
    // see a promotion of a variable assigned inside `try`
    final picked = maybe;
    if (picked == null || !mounted) return;
    setState(() {
      _fileName = picked.name;
      _step = _Step.uploading;
    });
    try {
      final api = ref.read(apiProvider);
      final handle = await api.uploadExport(picked.bytes);
      final summary = await api.archiveImport(handle, run: false);
      if (!mounted) return;
      setState(() {
        _handle = handle;
        _summary = summary;
        _step = _Step.preview;
      });
    } on CormexApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.message;
        _step = _Step.choose;
      });
    }
  }

  Future<void> _run() async {
    final handle = _handle;
    if (handle == null) return;
    setState(() => _step = _Step.running);
    try {
      final summary = await ref
          .read(apiProvider)
          .archiveImport(handle, run: true);
      if (!mounted) return;
      setState(() {
        _summary = summary;
        _step = _Step.done;
      });
    } on CormexApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _error = e.message;
        _step = _Step.preview;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final busy = _step == _Step.uploading || _step == _Step.running;
    return AlertDialog(
      title: const Text('导入聊天记录'),
      content: SizedBox(
        width: 460,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            if (_step == _Step.choose) ...[
              Text(
                '网页版和 App 里的聊天存在厂商的服务器上，Cormex 采集不到，'
                '需要先从厂商那里导出，再导入到这里。',
                style: theme.textTheme.bodyMedium,
              ),
              const SizedBox(height: 14),
              const _Where(name: 'ChatGPT', how: '设置 › 数据控制 › 导出数据，邮件里的 zip'),
              const _Where(name: 'Claude', how: '设置 › 隐私 › 导出数据，邮件里的 zip'),
              const _Where(name: 'Grok', how: 'accounts.x.ai/data 下载账户数据的 zip'),
              const _Where(
                name: 'Gemini',
                how: 'Google Takeout › 我的活动 › Gemini Apps，格式选 JSON',
              ),
              const SizedBox(height: 12),
              Text(
                '重复导入只写入新增和有变化的对话，已有的原样保留。'
                'Gemini 的导出不记录哪几问属于同一段对话，每一问会是一段单独的聊天。',
                style: theme.textTheme.bodySmall,
              ),
            ],
            if (busy) ...[
              Text(_step == _Step.uploading ? '正在上传并读取 $_fileName …' : '正在导入…'),
              const SizedBox(height: 12),
              const LinearProgressIndicator(),
            ],
            if (_summary case final s?
                when _step == _Step.preview || _step == _Step.done) ...[
              Text(
                _step == _Step.done
                    ? '已导入 ${s.source} 的聊天'
                    : '$_fileName：${s.source} 导出，共 ${s.conversations} 段对话',
                style: theme.textTheme.titleMedium,
              ),
              const SizedBox(height: 12),
              _Count(label: '新增', value: s.newCount),
              _Count(label: '有变化（保留旧版本，另存新版本）', value: s.changed),
              _Count(label: '已在档案中，跳过', value: s.unchanged),
              if (_step == _Step.preview && s.toWrite == 0) ...[
                const SizedBox(height: 10),
                Text(
                  '这份文件里的对话都已在档案中，没有要写入的。',
                  style: theme.textTheme.bodySmall,
                ),
              ],
            ],
            if (_error != null) ...[
              const SizedBox(height: 12),
              Text(
                _error!,
                style: theme.textTheme.bodySmall?.copyWith(
                  color: theme.colorScheme.error,
                ),
              ),
            ],
          ],
        ),
      ),
      actions: switch (_step) {
        _Step.choose => [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('取消'),
          ),
          FilledButton.icon(
            onPressed: _choose,
            icon: const Icon(Icons.upload_file, size: 18),
            label: const Text('选择文件'),
          ),
        ],
        _Step.uploading || _Step.running => const [],
        _Step.preview => [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('取消'),
          ),
          FilledButton(
            onPressed: (_summary?.toWrite ?? 0) == 0 ? null : _run,
            child: Text('导入 ${_summary?.toWrite ?? 0} 段'),
          ),
        ],
        _Step.done => [
          FilledButton(
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('完成'),
          ),
        ],
      },
    );
  }
}

class _Where extends StatelessWidget {
  const _Where({required this.name, required this.how});
  final String name;
  final String how;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.only(bottom: 6),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 72,
            child: Text(
              name,
              style: theme.textTheme.bodyMedium?.copyWith(
                fontWeight: FontWeight.w600,
              ),
            ),
          ),
          Expanded(child: Text(how, style: theme.textTheme.bodyMedium)),
        ],
      ),
    );
  }
}

class _Count extends StatelessWidget {
  const _Count({required this.label, required this.value});
  final String label;
  final int value;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.only(bottom: 4),
      child: Row(
        children: [
          Expanded(child: Text(label, style: theme.textTheme.bodyMedium)),
          Text(
            '$value',
            style: theme.textTheme.bodyMedium?.copyWith(
              fontWeight: FontWeight.w600,
              fontFeatures: const [FontFeature.tabularFigures()],
            ),
          ),
        ],
      ),
    );
  }
}
