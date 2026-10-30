import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../api/cormex_api.dart';
import '../../auth/credential_store.dart';
import '../../state/connection_controller.dart';

/// 凭据的两种形态。
///
/// 服务端的认证中间件两种都当 `Authorization: Bearer` 收（见
/// `cormexd::auth::require`），差别只在**这一把是从哪儿来的**：
/// 一个是运维手上那串，一个要拿账号密码去换。
enum _CredentialMode { token, password }

/// 连接页。
///
/// # 默认只问账号密码
///
/// 这一页曾经一上来就是三样东西：服务地址、令牌/密码的切换器、然后才是输入框。
/// 三样里有两样，**对着浏览器打开这一页的人来说都不需要**：
///
/// - 地址：Web 与 API 同源，页面自己就知道自己在哪（[CormexApi.canUseSameOrigin]）
/// - 令牌：那是运维路径 —— 部署时生成一次、用来开第一个账号。
///   日常登录的人手上没有它，也不该有
///
/// 所以两样都退到下面那行小字里，默认视图就是用户名 + 密码 + 登录。
/// 退到后面**不是删掉**：自托管的形态太多（地址在别处、认证关着、还没有账号），
/// 少一条出路就会有人卡在一个连报错都没有的页面上。
///
/// # 为什么这里仍然没有硬编码的地址
///
/// Cormex 是自托管的：每个人的服务都在不同的地方。地址的默认值来自
/// **构建期的 `CORMEX_BASE_URL`**（Web 镜像里是 `/api` 这个相对前缀），
/// 而不是源码里的某个域名 —— 那样一个镜像才对谁的部署都对。
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

  _CredentialMode _mode = _CredentialMode.password;
  bool _revealToken = false;

  /// 地址栏默认藏起来 —— **但只在藏了还连得上的地方**。
  ///
  /// 桌面端没有同源，藏起来等于让人对着一个连不上的按钮点，而错误信息会是
  /// 「连接被拒绝 127.0.0.1:8080」—— 读起来像服务没起来，不像少填了一项。
  late bool _showAddress = !CormexApi.canUseSameOrigin;

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
    final byToken = _mode == _CredentialMode.token;

    return Scaffold(
      body: Center(
        child: SingleChildScrollView(
          padding: const EdgeInsets.all(28),
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 400),
            // `autofillHints` 光写在输入框上不够 —— Flutter 要有这一层才会把
            // 它们真的翻译成浏览器认得的 autocomplete 属性。少了它，密码管理器
            // 既不填也不问要不要存，而两个输入框看起来一模一样。
            child: AutofillGroup(
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
                    '看它记住了什么、从哪儿听来的，以及任意一个过去的时刻'
                    '它都知道些什么。',
                    style: theme.textTheme.bodySmall,
                  ),
                  const SizedBox(height: 26),

                  if (_showAddress) ...[
                    TextField(
                      controller: _baseUrl,
                      autofocus: true,
                      enabled: !busy,
                      keyboardType: TextInputType.url,
                      decoration: const InputDecoration(
                        labelText: '服务地址',
                        hintText: 'http://127.0.0.1:8080',
                        prefixIcon: Icon(Icons.dns_outlined, size: 18),
                      ),
                      onSubmitted: (_) => _submit(),
                    ),
                    const SizedBox(height: 6),
                    // 空串在浏览器里是**有意义的配置**（同源），不是「没填」。
                    // 不说清楚的话，反代部署下的人会以为自己漏填了一项。
                    Text(
                      '留空：浏览器里表示与本页同源；桌面端表示 '
                      'http://127.0.0.1:8080。也可以只写路径前缀，如 /api。',
                      style: theme.textTheme.labelSmall,
                    ),
                    const SizedBox(height: 18),
                  ],

                  if (byToken) ...[
                    TextField(
                      controller: _token,
                      autofocus: !_showAddress,
                      enabled: !busy,
                      obscureText: !_revealToken,
                      decoration: InputDecoration(
                        labelText: '访问令牌',
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
                      '部署时用 `cormexd --generate-token` 生成，只显示一次。'
                      '认证关着（CORMEX_AUTH=disabled）时留空即可。',
                      style: theme.textTheme.labelSmall,
                    ),
                  ] else ...[
                    TextField(
                      controller: _username,
                      autofocus: !_showAddress,
                      enabled: !busy,
                      // 浏览器的密码管理器靠这两个 hint 认出这是一对登录框。
                      // 不给的话它既不会填也不会存 —— 而每次都手打密码的人，
                      // 迟早会挑一个好打的密码
                      autofillHints: const [AutofillHints.username],
                      decoration: const InputDecoration(
                        labelText: '用户名',
                        prefixIcon: Icon(
                          Icons.person_outline_rounded,
                          size: 18,
                        ),
                      ),
                      textInputAction: TextInputAction.next,
                    ),
                    const SizedBox(height: 12),
                    TextField(
                      controller: _password,
                      enabled: !busy,
                      obscureText: true,
                      autofillHints: const [AutofillHints.password],
                      decoration: const InputDecoration(
                        labelText: '密码',
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

                  const SizedBox(height: 22),
                  FilledButton(
                    onPressed: busy ? null : _submit,
                    child: busy
                        ? const SizedBox(
                            width: 17,
                            height: 17,
                            child: CircularProgressIndicator(strokeWidth: 2.2),
                          )
                        : Text(byToken ? '连接' : '登录'),
                  ),

                  // 两条出路。**做成小字链接而不是切换器**：切换器把「有两种登录
                  // 方式」变成每个人都要先读一遍的选择题，而其中一种九成的人用不上。
                  const SizedBox(height: 6),
                  Wrap(
                    alignment: WrapAlignment.center,
                    children: [
                      TextButton(
                        onPressed: busy
                            ? null
                            : () => setState(() {
                                _mode = byToken
                                    ? _CredentialMode.password
                                    : _CredentialMode.token;
                              }),
                        child: Text(byToken ? '用账号密码' : '用访问令牌'),
                      ),
                      if (!_showAddress)
                        TextButton(
                          onPressed: busy
                              ? null
                              : () => setState(() => _showAddress = true),
                          child: const Text('换个服务地址'),
                        ),
                    ],
                  ),

                  const SizedBox(height: 10),
                  Text(
                    kCredentialStorageNote,
                    style: theme.textTheme.labelSmall,
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}
