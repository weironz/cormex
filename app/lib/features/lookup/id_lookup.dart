import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../api/api_exception.dart';
import '../../models/session_memory.dart';
import '../../state/connection_controller.dart';
import '../memory/widgets/episode_sheet.dart';
import '../sessions/session_detail_page.dart';

/// 顶栏上的「按 ID 定位」按钮。
class IdLookupButton extends ConsumerWidget {
  const IdLookupButton({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) => IconButton(
    key: const ValueKey('id-lookup'),
    tooltip: '按 ID 定位会话或记忆',
    icon: const Icon(Icons.manage_search_rounded),
    onPressed: () => showIdLookup(context, ref),
  );
}

/// 粘贴一个 ID，打开它指向的会话或记忆。
///
/// 会话与记忆的 ID 来自同一个 ULID 空间，看形状分不出是哪一种，所以交给
/// 服务端去查（`GET /lookup/{id}`），而不是让人先选「这是会话还是记忆」。
Future<void> showIdLookup(BuildContext context, WidgetRef ref) async {
  final target = await showDialog<IdLookup>(
    context: context,
    builder: (_) => const _LookupDialog(),
  );
  if (target == null || !context.mounted) return;
  await openLookup(context, ref, target);
}

Future<void> openLookup(
  BuildContext context,
  WidgetRef ref,
  IdLookup target,
) async {
  final nav = Navigator.of(context);
  void openSession(String id) => nav.push(
    MaterialPageRoute<void>(builder: (_) => SessionDetailPage(sessionId: id)),
  );
  switch (target.kind) {
    case IdKind.session || IdKind.summary:
      if (target.sessionId case final String id) openSession(id);
    case IdKind.episode:
      await showEpisodeSheet(context, target.id);
    case IdKind.memory:
      final session = target.sessionId;
      if (session == null) return;
      final facts = await ref.read(apiProvider).sessionMemories(session);
      if (!context.mounted) return;
      final fact = facts.where((f) => f.id == target.id).firstOrNull;
      if (fact?.sourceEpisodeId case final String episode) {
        await showEpisodeSheet(context, episode, memory: fact);
      } else {
        openSession(session);
      }
  }
}

class _LookupDialog extends ConsumerStatefulWidget {
  const _LookupDialog();

  @override
  ConsumerState<_LookupDialog> createState() => _LookupDialogState();
}

class _LookupDialogState extends ConsumerState<_LookupDialog> {
  final _input = TextEditingController();
  String? _error;
  bool _busy = false;

  @override
  void dispose() {
    _input.dispose();
    super.dispose();
  }

  Future<void> _submit() async {
    final id = _input.text.trim();
    if (!ulidPattern.hasMatch(id)) {
      setState(() => _error = 'ID 是 26 位字母和数字，例如 01JC7…');
      return;
    }
    setState(() {
      _busy = true;
      _error = null;
    });
    try {
      final found = await ref.read(apiProvider).lookupId(id);
      if (mounted) Navigator.of(context).pop(found);
    } on CormexApiException catch (e) {
      if (mounted) {
        setState(() {
          _busy = false;
          _error = e.message;
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) => AlertDialog(
    title: const Text('按 ID 定位'),
    content: SizedBox(
      width: 420,
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Text('粘贴会话、记忆或摘要的 ID，直接打开它。'),
          const SizedBox(height: 12),
          TextField(
            key: const ValueKey('id-lookup-input'),
            controller: _input,
            autofocus: true,
            enabled: !_busy,
            onSubmitted: (_) => _submit(),
            style: const TextStyle(fontFamily: 'monospace'),
            decoration: InputDecoration(
              hintText: '01JC7…（26 位）',
              errorText: _error,
              errorMaxLines: 3,
            ),
          ),
        ],
      ),
    ),
    actions: [
      TextButton(
        onPressed: _busy ? null : () => Navigator.of(context).pop(),
        child: const Text('取消'),
      ),
      FilledButton(
        key: const ValueKey('id-lookup-submit'),
        onPressed: _busy ? null : _submit,
        child: _busy
            ? const SizedBox(
                width: 16,
                height: 16,
                child: CircularProgressIndicator(strokeWidth: 2),
              )
            : const Text('打开'),
      ),
    ],
  );
}
