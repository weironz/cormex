import 'package:flutter/material.dart';

class ArchiveSourceLabel extends StatelessWidget {
  const ArchiveSourceLabel({super.key});

  @override
  Widget build(BuildContext context) => Tooltip(
    message: '此聊天曾从来源应用的已归档聊天中采集，在 Cormex 中按正常聊天展示；不代表来源应用当前的归档状态。',
    child: Text(
      '来自已归档聊天',
      style: Theme.of(context).textTheme.labelSmall?.copyWith(
        color: Theme.of(context).colorScheme.secondary,
      ),
    ),
  );
}
