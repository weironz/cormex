import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../api/api_exception.dart';
import '../../../state/connection_controller.dart';

/// 原始字节的查看器。
///
/// # 「解除遮罩」必须二次确认
///
/// 默认显示的是**遮罩后**的字节：疑似密钥已经换成 `[masked:<rule>]`。点一下
/// 就原样返回原文，意味着屏幕上会出现一把真的钥匙 —— 而屏幕会被截图、会被
/// 投屏、会被拍下来。所以那个按钮要再问一次，并且说明它会写一条审计日志。
///
/// # 按字节显示，不按文本解码
///
/// 这是文件字节，不保证是 UTF-8。按文本解码会把非 UTF-8 的部分弄坏，而
/// 原文的用途恰恰是「原样」。
class RawView extends ConsumerStatefulWidget {
  const RawView({super.key, required this.generationId});

  final String generationId;

  @override
  ConsumerState<RawView> createState() => _RawViewState();
}

class _RawViewState extends ConsumerState<RawView> {
  Uint8List? _bytes;
  String? _error;
  bool _loading = true;
  bool _unmasked = false;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    setState(() => _loading = true);
    try {
      final bytes = await ref
          .read(apiProvider)
          .rawSlice(widget.generationId, unmask: _unmasked);
      if (!mounted) return;
      setState(() {
        _bytes = bytes;
        _loading = false;
        _error = null;
      });
    } on CormexApiException catch (e) {
      if (!mounted) return;
      setState(() {
        _loading = false;
        _error = e.message;
      });
    }
  }

  Future<void> _confirmUnmask() async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('显示未遮罩的原文？'),
        content: const Text(
          '这会原样显示文件里的内容，包括疑似密钥。\n\n'
          '屏幕会被截图、会被投屏、会被拍下来。这次读取会记进访问日志。',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('取消'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('显示原文'),
          ),
        ],
      ),
    );
    if (ok != true) return;
    setState(() => _unmasked = true);
    await _load();
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      appBar: AppBar(
        title: const Text('原始记录'),
        actions: [
          if (!_unmasked)
            TextButton.icon(
              onPressed: _loading ? null : _confirmUnmask,
              icon: const Icon(Icons.lock_open, size: 18),
              label: const Text('解除遮罩'),
            )
          else
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 12),
              child: Center(
                child: Text(
                  '未遮罩',
                  style: theme.textTheme.labelMedium?.copyWith(
                    color: theme.colorScheme.error,
                  ),
                ),
              ),
            ),
        ],
      ),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : _error != null
          ? Center(
              child: Padding(
                padding: const EdgeInsets.all(24),
                child: Text(_error!, textAlign: TextAlign.center),
              ),
            )
          : Scrollbar(
              child: SingleChildScrollView(
                padding: const EdgeInsets.all(16),
                child: SelectableText(
                  // 解码失败时给出字节数而不是一句「解码失败」：至少让人知道
                  // 内容有多大，以及它不是文本
                  _bytes == null
                      ? ''
                      : _tryDecode(_bytes!),
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
                ),
              ),
            ),
    );
  }

  String _tryDecode(Uint8List bytes) {
    try {
      return utf8.decode(bytes);
    } on FormatException {
      return '[${bytes.length} 字节，不是 UTF-8 文本]';
    }
  }
}
