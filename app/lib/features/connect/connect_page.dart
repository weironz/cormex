import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../auth/credential_store.dart';
import '../../state/connection_controller.dart';

/// 凭据的两种形态。
///
/// 服务端的认证中间件两种都当 `Authorization: Bearer` 收（见
/// `cortexd::auth::require`），差别只在**这一把是从哪儿来的**：
/// 一个是运维手上那串，一个要拿账号密码去换。
enum _CredentialMode { token, password }

/// 连接页 —— 填后端地址与凭据。
///
/// # 为什么这里没有硬编码的地址
///
/// Cormex 是自托管的：每个人的 cortexd 都在不同的地方（本机 8080、内网某台、
/// 反代到 `https://…/api`）。把地址烤进构建里，等于让每个部署者都得自己编
/// 一份客户端。
class ConnectPage extends ConsumerStatefulWidget {
  const ConnectPage({super.key});

  @override
  ConsumerState<ConnectPage> createState() => _ConnectPageState();
}

class _ConnectPageState extends ConsumerState<ConnectPage> {
  late final TextEditingController _baseUrl = TextEditingController(
    text: ref.read(connectionProvider).baseUrl,
  );
  final _token = TextEditingController();
  final _username = TextEditingController();
  final _password = TextEditingController();

  _CredentialMode _mode = _CredentialMode.token;
  bool _revealToken = false;

  @override
  void dispose() {
    _baseUrl.dispose();
    _token.dispose();
    _username.dispose();
    _password.dispose();
    super.dispose();
  }

  void _submit() {
    final notifier = ref.read(connectionProvider.notifier);
    final baseUrl = _baseUrl.text.trim();
    switch (_mode) {
      case _CredentialMode.token:
        notifier.connectWithToken(baseUrl: baseUrl, token: _token.text);
      case _CredentialMode.password:
        notifier.connectWithPassword(
          baseUrl: baseUrl,
          username: _username.text.trim(),
          password: _password.text,
        );
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final conn = ref.watch(connectionProvider);
    final busy = conn.phase == ConnectionPhase.connecting;

    return Scaffold(
      body: Center(
        child: SingleChildScrollView(
          padding: const EdgeInsets.all(28),
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 440),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Row(
                  children: [
                    Icon(
                      Icons.psychology_rounded,
                      size: 26,
                      color: scheme.secondary,
                    ),
                    const SizedBox(width: 10),
                    Text('Cormex', style: theme.textTheme.headlineSmall),
                  ],
                ),
                const SizedBox(height: 6),
                Text(
                  '连上你的记忆服务，看它记住了什么、从哪儿听来的、'
                  '以及任意一个过去的时刻它都知道些什么。',
                  style: theme.textTheme.bodySmall,
                ),
                const SizedBox(height: 26),

                Text('服务地址', style: theme.textTheme.titleSmall),
                const SizedBox(height: 7),
                TextField(
                  controller: _baseUrl,
                  autofocus: true,
                  enabled: !busy,
                  keyboardType: TextInputType.url,
                  decoration: const InputDecoration(
                    hintText: 'http://127.0.0.1:8080',
                    prefixIcon: Icon(Icons.dns_outlined, size: 18),
                  ),
                  onSubmitted: (_) => _submit(),
                ),
                const SizedBox(height: 6),
                // 空串在浏览器里是**有意义的配置**（同源反代），不是「没填」。
                // 不说清楚的话，反代部署下的人会以为自己漏填了一项。
                Text(
                  '留空：浏览器里表示与本页同源（反代到同一个域名下时用这个）；'
                  '桌面端表示 http://127.0.0.1:8080。',
                  style: theme.textTheme.labelSmall,
                ),
                const SizedBox(height: 20),

                SegmentedButton<_CredentialMode>(
                  segments: const [
                    ButtonSegment(
                      value: _CredentialMode.token,
                      label: Text('访问令牌'),
                      icon: Icon(Icons.key_rounded, size: 15),
                    ),
                    ButtonSegment(
                      value: _CredentialMode.password,
                      label: Text('账号密码'),
                      icon: Icon(Icons.person_outline_rounded, size: 15),
                    ),
                  ],
                  selected: {_mode},
                  showSelectedIcon: false,
                  onSelectionChanged: busy
                      ? null
                      : (s) => setState(() => _mode = s.first),
                ),
                const SizedBox(height: 14),

                if (_mode == _CredentialMode.token) ...[
                  TextField(
                    controller: _token,
                    enabled: !busy,
                    obscureText: !_revealToken,
                    decoration: InputDecoration(
                      hintText: '$kTokenEnvVar=… 里的那一串',
                      prefixIcon: const Icon(Icons.key_rounded, size: 18),
                      suffixIcon: IconButton(
                        iconSize: 17,
                        tooltip: _revealToken ? '隐藏' : '显示',
                        icon: Icon(
                          _revealToken
                              ? Icons.visibility_off_rounded
                              : Icons.visibility_rounded,
                        ),
                        onPressed: () =>
                            setState(() => _revealToken = !_revealToken),
                      ),
                    ),
                    onSubmitted: (_) => _submit(),
                  ),
                  const SizedBox(height: 6),
                  Text(
                    '运维那边用 `cortexd --generate-token` 生成，只显示一次。'
                    '部署若设了 CORTEX_AUTH=disabled，这一栏留空即可。',
                    style: theme.textTheme.labelSmall,
                  ),
                ] else ...[
                  TextField(
                    controller: _username,
                    enabled: !busy,
                    decoration: const InputDecoration(
                      hintText: '用户名',
                      prefixIcon: Icon(Icons.person_outline_rounded, size: 18),
                    ),
                  ),
                  const SizedBox(height: 10),
                  TextField(
                    controller: _password,
                    enabled: !busy,
                    obscureText: true,
                    decoration: const InputDecoration(
                      hintText: '密码',
                      prefixIcon: Icon(Icons.lock_outline_rounded, size: 18),
                    ),
                    onSubmitted: (_) => _submit(),
                  ),
                ],

                if (conn.error case final error?) ...[
                  const SizedBox(height: 16),
                  Container(
                    padding: const EdgeInsets.all(12),
                    decoration: BoxDecoration(
                      color: scheme.errorContainer.withValues(alpha: 0.55),
                      borderRadius: BorderRadius.circular(10),
                      border: Border.all(
                        color: scheme.error.withValues(alpha: 0.35),
                      ),
                    ),
                    // 可选中：服务端写给用户看的那句话有时要贴给别人看。
                    child: SelectableText(
                      error,
                      style: theme.textTheme.bodySmall?.copyWith(
                        color: scheme.onErrorContainer,
                      ),
                    ),
                  ),
                ],

                const SizedBox(height: 20),
                FilledButton(
                  onPressed: busy ? null : _submit,
                  child: busy
                      ? const SizedBox(
                          width: 17,
                          height: 17,
                          child: CircularProgressIndicator(strokeWidth: 2.2),
                        )
                      : const Text('连接'),
                ),
                const SizedBox(height: 14),
                Text(kCredentialStorageNote, style: theme.textTheme.labelSmall),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
