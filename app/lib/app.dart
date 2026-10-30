import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import 'core/theme.dart';
import 'features/connect/connect_page.dart';
import 'features/home/home_page.dart';
import 'state/connection_controller.dart';

class CormexApp extends StatelessWidget {
  const CormexApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Cormex',
      debugShowCheckedModeBanner: false,
      theme: CormexTheme.light(),
      darkTheme: CormexTheme.dark(),
      home: const _Gate(),
    );
  }
}

/// 连上之前给连接页，连上之后给记忆浏览器。
class _Gate extends ConsumerWidget {
  const _Gate();

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final phase = ref.watch(connectionProvider.select((s) => s.phase));

    return switch (phase) {
      // 启动时正在读凭据库。这一档必须与 disconnected 分开，否则每次启动
      // 都会先闪一下连接页，然后在恢复成功后跳走 —— 而恢复通常只要几十毫秒，
      // 那一闪看起来就是个 bug。
      ConnectionPhase.booting => const Scaffold(
        body: Center(
          child: SizedBox(
            width: 24,
            height: 24,
            child: CircularProgressIndicator(strokeWidth: 2.4),
          ),
        ),
      ),
      ConnectionPhase.connected => const HomePage(),
      // connecting 也留在连接页：那上面有按钮的忙碌态和错误框，
      // 换一块转圈的白屏只会让失败时没地方显示原因。
      _ => const ConnectPage(),
    };
  }
}
