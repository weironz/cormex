import 'package:flutter/material.dart';

class EmptyState extends StatelessWidget {
  const EmptyState({
    super.key,
    required this.icon,
    required this.title,
    this.description,
    this.action,
    this.tone = EmptyStateTone.neutral,
  });

  final IconData icon;
  final String title;
  final String? description;
  final Widget? action;
  final EmptyStateTone tone;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final accent = switch (tone) {
      EmptyStateTone.neutral => scheme.onSurfaceVariant,
      EmptyStateTone.error => scheme.error,
    };

    return Center(
      child: Padding(
        padding: const EdgeInsets.all(28),
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 380),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Container(
                width: 46,
                height: 46,
                decoration: BoxDecoration(
                  color: accent.withValues(alpha: 0.10),
                  borderRadius: BorderRadius.circular(13),
                ),
                child: Icon(icon, size: 22, color: accent),
              ),
              const SizedBox(height: 14),
              Text(
                title,
                textAlign: TextAlign.center,
                style: theme.textTheme.titleMedium,
              ),
              if (description != null) ...[
                const SizedBox(height: 6),
                Text(
                  description!,
                  textAlign: TextAlign.center,
                  style: theme.textTheme.bodySmall,
                ),
              ],
              if (action != null) ...[const SizedBox(height: 18), action!],
            ],
          ),
        ),
      ),
    );
  }
}

/// 空结果与错误在这个产品里必须长得不一样。
///
/// 检索器判定「与已存记忆无关」时会主动弃权，返回空是**正确结果**；
/// 把它涂成错误色，等于训练用户不相信一个对的答案。
enum EmptyStateTone { neutral, error }
