import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

/// A record's id, short enough to sit in a card, copied whole on tap.
///
/// Sessions and memories are both ULIDs. The tail is shown, not the head: the
/// head is a timestamp, and ten records from one afternoon share it, while the
/// tail is the random part that tells them apart at a glance.
class IdChip extends StatelessWidget {
  const IdChip({super.key, required this.id, this.label = 'ID'});
  final String id;
  final String label;

  static String short(String id) =>
      id.length > 8 ? '…${id.substring(id.length - 8)}' : id;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    return Tooltip(
      message: '$label $id\n点击复制完整 ID',
      child: InkWell(
        key: ValueKey('id-chip-$id'),
        borderRadius: BorderRadius.circular(6),
        onTap: () => copyId(context, id, label: label),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 4, vertical: 2),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(
                '$label ${short(id)}',
                style: theme.textTheme.labelSmall?.copyWith(
                  color: scheme.onSurfaceVariant,
                  fontFamily: 'monospace',
                  fontFeatures: const [FontFeature.tabularFigures()],
                ),
              ),
              const SizedBox(width: 3),
              Icon(
                Icons.copy_rounded,
                size: 11,
                color: scheme.onSurfaceVariant,
              ),
            ],
          ),
        ),
      ),
    );
  }
}

Future<void> copyId(
  BuildContext context,
  String id, {
  String label = 'ID',
}) async {
  final messenger = ScaffoldMessenger.maybeOf(context);
  try {
    await Clipboard.setData(ClipboardData(text: id));
    messenger?.showSnackBar(SnackBar(content: Text('已复制$label：$id')));
  } catch (_) {
    messenger?.showSnackBar(const SnackBar(content: Text('复制失败，请手动选择')));
  }
}

/// A full id on one line with a copy button, for detail views.
class IdLine extends StatelessWidget {
  const IdLine({super.key, required this.label, required this.id});
  final String label, id;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Row(
      children: [
        Text('$label：', style: theme.textTheme.bodySmall),
        Flexible(
          child: SelectableText(
            id,
            maxLines: 1,
            style: theme.textTheme.bodySmall?.copyWith(fontFamily: 'monospace'),
          ),
        ),
        IconButton(
          tooltip: '复制$label',
          visualDensity: VisualDensity.compact,
          iconSize: 14,
          onPressed: () => copyId(context, id, label: label),
          icon: const Icon(Icons.copy_rounded),
        ),
      ],
    );
  }
}
