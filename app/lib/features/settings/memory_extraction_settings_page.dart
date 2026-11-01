import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../../api/api_exception.dart';
import '../../models/memory_extraction_status.dart';
import '../../state/connection_controller.dart';
import '../../state/memory_extraction.dart';
import 'memory_trial_section.dart';

class MemoryExtractionSettingsPage extends ConsumerWidget {
  const MemoryExtractionSettingsPage({super.key});
  @override
  Widget build(BuildContext context, WidgetRef ref) => Scaffold(
    appBar: AppBar(title: const Text('模型与记忆提取')),
    body: ref
        .watch(memoryExtractionProvider)
        .when(
          loading: () => const Center(child: CircularProgressIndicator()),
          error: (e, _) => Center(
            child: TextButton(
              onPressed: () => ref.invalidate(memoryExtractionProvider),
              child: const Text('读取失败，点击重试'),
            ),
          ),
          data: (s) => _ExtractionForm(initial: s),
        ),
  );
}

class _ExtractionForm extends ConsumerStatefulWidget {
  const _ExtractionForm({required this.initial});
  final MemoryExtractionStatus initial;
  @override
  ConsumerState<_ExtractionForm> createState() => _ExtractionFormState();
}

class _ExtractionFormState extends ConsumerState<_ExtractionForm> {
  late bool _enabled = widget.initial.enabled;
  late bool _server = widget.initial.useServerModel;
  late String _provider = widget.initial.provider;
  late final _model = TextEditingController(text: widget.initial.model);
  late final _url = TextEditingController(text: widget.initial.baseUrl ?? '');
  final _key = TextEditingController();
  bool _clearKey = false, _saving = false, _testing = false, _dirty = false;
  String? _error, _testResult;
  @override
  void dispose() {
    _model.dispose();
    _url.dispose();
    _key.dispose();
    super.dispose();
  }

  Future<void> _save() async {
    setState(() {
      _saving = true;
      _error = null;
      _testResult = null;
    });
    try {
      await ref.read(apiProvider).saveMemoryExtraction({
        'enabled': _enabled,
        'use_server_model': _server,
        'provider': _provider,
        'model': _model.text.trim(),
        'base_url': _url.text.trim(),
        if (_key.text.isNotEmpty) 'api_key': _key.text,
        'clear_api_key': _clearKey,
      });
      _key.clear();
      _dirty = false;
      ref.invalidate(memoryExtractionProvider);
      if (mounted) {
        ScaffoldMessenger.of(
          context,
        ).showSnackBar(const SnackBar(content: Text('记忆提取设置已保存')));
      }
    } on CormexApiException catch (e) {
      if (mounted) setState(() => _error = e.message);
    } finally {
      if (mounted) setState(() => _saving = false);
    }
  }

  Future<void> _test() async {
    setState(() {
      _testing = true;
      _error = null;
      _testResult = null;
    });
    try {
      await ref.read(apiProvider).testMemoryExtractionModel();
      if (mounted) setState(() => _testResult = '连接成功，模型已响应。');
    } on CormexApiException catch (e) {
      if (mounted) setState(() => _error = e.message);
    } finally {
      if (mounted) setState(() => _testing = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final s = widget.initial;
    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: 720),
        child: ListView(
          padding: const EdgeInsets.all(24),
          children: [
            Text('从完整问答整理记忆与摘要', style: theme.textTheme.headlineSmall),
            const SizedBox(height: 8),
            const Text('设置保存到当前账号，在其他设备登录后同样生效。暂停提取不会删除聊天或已有记忆。'),
            const SizedBox(height: 20),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('全量自动提取'),
              subtitle: const Text('开启后会处理所有已有和新增聊天。小样本验证阶段请保持关闭。'),
              value: _enabled,
              onChanged: _saving
                  ? null
                  : (v) => setState(() {
                      _enabled = v;
                      _dirty = true;
                    }),
            ),
            const Divider(height: 32),
            Text('提取模型', style: theme.textTheme.titleLarge),
            const SizedBox(height: 16),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              title: const Text('使用服务端默认模型'),
              subtitle: Text(
                _server
                    ? (s.useServerModel
                          ? '${s.provider} / ${s.model}'
                          : '保存后使用服务端配置的提取模型。')
                    : '仅用于聊天档案的记忆提取，不更改其他模型配置。',
              ),
              value: _server,
              onChanged: _saving
                  ? null
                  : (v) => setState(() {
                      _server = v;
                      _dirty = true;
                    }),
            ),
            if (!_server) ...[
              const SizedBox(height: 16),
              DropdownButtonFormField<String>(
                initialValue: _provider,
                decoration: const InputDecoration(labelText: '服务商'),
                isExpanded: true,
                items: {...s.providers, _provider}
                    .map((p) => DropdownMenuItem(value: p, child: Text(p)))
                    .toList(),
                onChanged: _saving
                    ? null
                    : (v) {
                        if (v != null) {
                          setState(() {
                            _provider = v;
                            _dirty = true;
                          });
                        }
                      },
              ),
              const SizedBox(height: 18),
              TextField(
                controller: _model,
                onChanged: (_) => setState(() => _dirty = true),
                decoration: const InputDecoration(
                  labelText: '模型名称',
                  hintText: '填写服务商提供的模型 ID',
                ),
              ),
              const SizedBox(height: 18),
              TextField(
                controller: _url,
                onChanged: (_) => setState(() => _dirty = true),
                keyboardType: TextInputType.url,
                decoration: const InputDecoration(
                  labelText: '服务地址',
                  helperText: '留空使用官方地址。OpenAI 兼容网关选择 openai。',
                  helperMaxLines: 2,
                ),
              ),
              const SizedBox(height: 18),
              TextField(
                controller: _key,
                obscureText: true,
                autocorrect: false,
                enableSuggestions: false,
                onChanged: (_) => setState(() => _dirty = true),
                decoration: InputDecoration(
                  labelText: 'API Key',
                  helperText: s.keyConfigured
                      ? '已加密保存（尾号 ${s.keyTail ?? '—'}）。留空保留，更换服务商或地址需重新填写。'
                      : '加密保存在服务端；仅 Ollama 等无鉴权服务可以留空。',
                  helperMaxLines: 3,
                ),
              ),
              if (s.keyConfigured)
                CheckboxListTile(
                  contentPadding: EdgeInsets.zero,
                  title: const Text('清除已保存的 API Key'),
                  value: _clearKey,
                  onChanged: _saving
                      ? null
                      : (v) => setState(() {
                          _clearKey = v ?? false;
                          _dirty = true;
                        }),
                ),
            ] else ...[
              const SizedBox(height: 12),
              Text(
                s.useServerModel
                    ? '服务地址：${s.baseUrl ?? '服务商默认地址'}'
                    : '默认模型和服务地址由服务端管理，保存后生效。',
              ),
            ],
            const SizedBox(height: 18),
            const Text('模型调用可能产生费用。连接测试只发送固定短提示，不发送聊天历史；测试前请先保存配置。'),
            if (_error != null) ...[
              const SizedBox(height: 16),
              Text(_error!, style: TextStyle(color: theme.colorScheme.error)),
            ],
            if (_testResult != null) ...[
              const SizedBox(height: 16),
              Text(
                _testResult!,
                style: TextStyle(color: theme.colorScheme.primary),
              ),
            ],
            const SizedBox(height: 24),
            Wrap(
              spacing: 12,
              runSpacing: 12,
              children: [
                FilledButton.icon(
                  onPressed: _saving || _testing ? null : _save,
                  icon: _saving
                      ? const SizedBox(
                          width: 16,
                          height: 16,
                          child: CircularProgressIndicator(strokeWidth: 2),
                        )
                      : const Icon(Icons.save_outlined),
                  label: Text(_saving ? '保存中…' : '保存设置'),
                ),
                OutlinedButton(
                  onPressed: _saving || _testing || _dirty ? null : _test,
                  child: Text(_testing ? '测试中…' : '测试已保存的模型'),
                ),
              ],
            ),
            const SizedBox(height: 24),
            MemoryTrialSection(status: s, unsaved: _dirty),
          ],
        ),
      ),
    );
  }
}
