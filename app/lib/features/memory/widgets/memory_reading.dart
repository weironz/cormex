import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

Future<void> showMemoryReadingDialog(
  BuildContext context, {
  required Widget child,
}) => showDialog<void>(
  context: context,
  builder: (context) {
    final size = MediaQuery.sizeOf(context);
    if (size.width < 720 || size.height < 520) {
      return Dialog.fullscreen(child: SafeArea(child: child));
    }
    return Dialog(
      insetPadding: const EdgeInsets.all(32),
      clipBehavior: Clip.antiAlias,
      child: SizedBox(
        width: math.min(size.width - 64, 1180),
        height: math.min(size.height * .90, 1100),
        child: child,
      ),
    );
  },
);

class MemoryTextPanel extends StatelessWidget {
  const MemoryTextPanel({super.key, required this.child});
  final Widget child;

  @override
  Widget build(BuildContext context) => Container(
    width: double.infinity,
    padding: const EdgeInsets.all(20),
    decoration: BoxDecoration(
      color: Theme.of(context).colorScheme.surfaceContainerLow,
      borderRadius: BorderRadius.circular(12),
      border: Border.all(color: Theme.of(context).colorScheme.outlineVariant),
    ),
    child: child,
  );
}

class MemoryMarkdown extends StatelessWidget {
  const MemoryMarkdown(this.text, {super.key});
  final String text;

  @override
  Widget build(BuildContext context) => MarkdownBody(
    data: text,
    selectable: true,
    softLineBreak: true,
    styleSheet: MarkdownStyleSheet.fromTheme(
      Theme.of(context),
    ).copyWith(p: Theme.of(context).textTheme.bodyLarge?.copyWith(height: 1.7)),
    imageBuilder: (uri, title, alt) =>
        Text('[图片${alt == null ? '' : ': $alt'}]'),
  );
}
