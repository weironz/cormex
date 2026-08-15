import 'dart:async';

import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../api/api_exception.dart';
import '../api/cormex_api.dart';
import '../auth/credential_store.dart';
import '../auth/stored_credential.dart';
import '../core/app_config.dart';
import '../models/account.dart';
import '../models/health_status.dart';

/// 连接处在哪一档。
enum ConnectionPhase {
  /// 还在从凭据库里恢复。**必须与 [disconnected] 分开**：两者都「没连上」，
  /// 但一个该转圈、另一个该给表单。合成一档的症状是应用每次启动都先闪一下
  /// 登录页，然后在恢复成功后跳走。
  booting,

  /// 没有可用凭据，或者刚断开。
  disconnected,

  /// 正在探测 / 换令牌。
  connecting,

  /// 通了。
  connected,
}

class ConnectionState {
  const ConnectionState({
    this.phase = ConnectionPhase.booting,
    this.baseUrl = '',
    this.bearer,
    this.credential,
    this.health,
    this.error,
  });

  final ConnectionPhase phase;

  /// cortexd 的地址。空串在浏览器里意味着**同源**（见
  /// [CormexApi.resolveBase]），在桌面端意味着回落到本机默认端口。
  final String baseUrl;

  /// 每个请求实际带的那把钥匙。
  ///
  /// 与 [credential] 不是一回事：存下来的可能是一个 refresh token，而它
  /// **不能**直接当请求凭据用 —— 那种情况下这里放的是换来的 access token。
  /// 认证被关掉的部署里两者都是 null。
  final String? bearer;

  /// 存进凭据库的那把（下次启动靠它）。
  final StoredCredential? credential;

  final HealthStatus? health;

  final String? error;

  bool get isConnected => phase == ConnectionPhase.connected;

  ConnectionState copyWith({
    ConnectionPhase? phase,
    String? baseUrl,
    Object? bearer = _sentinel,
    Object? credential = _sentinel,
    Object? health = _sentinel,
    Object? error = _sentinel,
  }) => ConnectionState(
    phase: phase ?? this.phase,
    baseUrl: baseUrl ?? this.baseUrl,
    bearer: bearer == _sentinel ? this.bearer : bearer as String?,
    credential: credential == _sentinel
        ? this.credential
        : credential as StoredCredential?,
    health: health == _sentinel ? this.health : health as HealthStatus?,
    error: error == _sentinel ? this.error : error as String?,
  );

  static const Object _sentinel = Object();
}

/// 连接 / 断开，以及 access token 的续期。
class ConnectionController extends Notifier<ConnectionState> {
  /// 同一时刻只允许一次续期在飞。
  ///
  /// 401 会在**每一条**在飞的请求上各响一次铃，而它们通常是一批同时发出去
  /// 的。不收拢的话，一次过期会引发 N 次 `/auth/refresh` —— 而服务端的重放
  /// 判定会把第二次之后的每一次都当成令牌泄露，**把整条链一起作废**。
  /// 也就是说，天真的实现会把「令牌过期」升级成「被强制登出」。
  Future<bool>? _refreshInFlight;

  @override
  ConnectionState build() {
    unawaited(_restore());
    return const ConnectionState();
  }

  /// 启动时从凭据库里恢复。
  Future<void> _restore() async {
    final storedUrl = await readBaseUrl();
    final credential = await readCredential();
    if (!ref.mounted) return;

    // 存过的优先，没存过用构建期默认值。**判据是 `?? `不是 `isEmpty`**：
    // 空串是一个合法的、用户可能真的存下来的配置（同源根）。用 `isEmpty`
    // 兜底，会让「在 Web 镜像里手动把地址清空」这个动作在下次启动时
    // 被静默还原成 `/api`。
    final baseUrl = storedUrl ?? AppConfig.defaultBaseUrl;
    if (credential == null) {
      // 地址仍然填回表单里：没有凭据不等于地址也白填一次。
      state = state.copyWith(
        phase: ConnectionPhase.disconnected,
        baseUrl: baseUrl,
      );
      return;
    }
    await connectWithCredential(baseUrl: baseUrl, credential: credential);
  }

  /// 用一把 bearer token 连（预共享 token，或者认证被关掉时的空串）。
  Future<void> connectWithToken({
    required String baseUrl,
    required String token,
  }) async {
    final trimmed = token.trim();
    await connectWithCredential(
      baseUrl: baseUrl,
      credential: trimmed.isEmpty
          ? null
          : StoredCredential(kind: CredentialKind.bearer, value: trimmed),
    );
  }

  /// 用账号密码连。
  Future<void> connectWithPassword({
    required String baseUrl,
    required String username,
    required String password,
  }) async {
    state = state.copyWith(
      phase: ConnectionPhase.connecting,
      baseUrl: baseUrl,
      error: null,
    );
    final probe = CormexApi(baseUrl: baseUrl);
    try {
      final health = await probe.health();
      final tokens = await probe.login(username, password);
      if (!ref.mounted) return;
      await _settle(
        baseUrl: baseUrl,
        health: health,
        bearer: tokens.accessToken,
        credential: StoredCredential(
          kind: CredentialKind.refresh,
          value: tokens.refreshToken,
        ),
      );
    } on CormexApiException catch (e) {
      if (ref.mounted) _fail(e.message);
    } on Object catch (e) {
      if (ref.mounted) _fail('$e');
    } finally {
      probe.dispose();
    }
  }

  /// 连接的主路径：探 `/health`，把凭据变成一把可用的 bearer，验一次，存下来。
  ///
  /// [credential] 为 null 表示「不带凭据」—— 只有在服务端报
  /// `auth: disabled` 时这才走得通，而那一判由服务端给（`/health` 的回答），
  /// 不由客户端猜。
  Future<void> connectWithCredential({
    required String baseUrl,
    required StoredCredential? credential,
  }) async {
    state = state.copyWith(
      phase: ConnectionPhase.connecting,
      baseUrl: baseUrl,
      error: null,
    );

    // 用一个**临时**客户端做整套握手，而不是先把状态写好让 `apiProvider`
    // 去建：握手失败时那个 provider 已经带着坏凭据建起来了，而界面上任何
    // 一个还活着的 widget 都可能已经拿它发过请求。
    final probe = CormexApi(baseUrl: baseUrl, token: _bearerOf(credential));
    try {
      final health = await probe.health();
      if (!ref.mounted) return;

      if (health.requiresToken && credential == null) {
        _fail('这个部署要凭据（/health 报 auth: ${health.auth}），但没有填。');
        return;
      }

      // refresh token 不能直接用来发请求，先换一对新的。
      var bearer = _bearerOf(credential);
      var toStore = credential;
      if (credential != null && credential.kind == CredentialKind.refresh) {
        final tokens = await probe.refreshSession(credential.value);
        if (!ref.mounted) return;
        bearer = tokens.accessToken;
        // **必须存新的那个**：服务端每次刷新都轮转，继续用旧的会被判成重放。
        toStore = StoredCredential(
          kind: CredentialKind.refresh,
          value: tokens.refreshToken,
        );
      }

      if (bearer != null) {
        // 用换到手的那把再验一次。省掉这一步的话，一把坏钥匙会一路走到
        // 记忆浏览器里，在那儿以「检索失败」的形式出现 —— 而真正的问题
        // 在上一个界面。
        final verifier = CormexApi(baseUrl: baseUrl, token: bearer);
        try {
          await verifier.verifyCredential();
        } finally {
          verifier.dispose();
        }
        if (!ref.mounted) return;
      }

      await _settle(
        baseUrl: baseUrl,
        health: health,
        bearer: bearer,
        credential: toStore,
      );
    } on CormexApiException catch (e) {
      if (ref.mounted) _fail(e.message);
    } on Object catch (e) {
      if (ref.mounted) _fail('$e');
    } finally {
      probe.dispose();
    }
  }

  Future<void> _settle({
    required String baseUrl,
    required HealthStatus health,
    required String? bearer,
    required StoredCredential? credential,
  }) async {
    state = state.copyWith(
      phase: ConnectionPhase.connected,
      baseUrl: baseUrl,
      bearer: bearer,
      credential: credential,
      health: health,
      error: null,
    );

    await writeBaseUrl(baseUrl);
    if (credential != null) await writeCredential(credential);
  }

  void _fail(String message) {
    state = state.copyWith(
      phase: ConnectionPhase.disconnected,
      bearer: null,
      error: message,
    );
  }

  /// 401 之后走这一条：能续就续，不能就断开。
  ///
  /// 由 [CormexApi.onUnauthorized] 触发。返回值只给测试与调用方看，界面
  /// 直接看状态。
  Future<bool> handleUnauthorized() {
    return _refreshInFlight ??= _refresh().whenComplete(() {
      _refreshInFlight = null;
    });
  }

  Future<bool> _refresh() async {
    final credential = state.credential;
    // 预共享 token 没有「续期」这回事 —— 它不过期，401 只意味着它不对
    // （被轮换掉了，或者一开始就填错了）。这里去尝试刷新只会多一次注定
    // 失败的往返。
    if (credential == null || credential.kind != CredentialKind.refresh) {
      _fail('凭据被拒（401）。请重新填一次。');
      return false;
    }

    final api = CormexApi(baseUrl: state.baseUrl);
    try {
      final tokens = await api.refreshSession(credential.value);
      if (!ref.mounted) return false;
      final rotated = StoredCredential(
        kind: CredentialKind.refresh,
        value: tokens.refreshToken,
      );
      state = state.copyWith(
        bearer: tokens.accessToken,
        credential: rotated,
        error: null,
      );
      await writeCredential(rotated);
      return true;
    } on Object catch (_) {
      if (ref.mounted) _fail('登录已过期，请重新连接。');
      return false;
    } finally {
      api.dispose();
    }
  }

  /// 断开：清本地那份，并且**让服务端也作废这条链**。
  ///
  /// 只清本地是不够的：本地删除只让这台机器忘了，而已经泄露出去的那一份
  /// 照样能用到 30 天后。
  Future<void> disconnect() async {
    final credential = state.credential;
    if (credential != null && credential.kind == CredentialKind.refresh) {
      final api = CormexApi(baseUrl: state.baseUrl, token: state.bearer);
      try {
        await api.logout(credential.value);
      } finally {
        api.dispose();
      }
    }
    await clearCredential();
    if (!ref.mounted) return;
    state = ConnectionState(
      phase: ConnectionPhase.disconnected,
      baseUrl: state.baseUrl,
    );
  }

  static String? _bearerOf(StoredCredential? c) =>
      (c != null && c.kind == CredentialKind.bearer) ? c.value : null;
}

final connectionProvider =
    NotifierProvider<ConnectionController, ConnectionState>(
      ConnectionController.new,
    );

/// 应用其余部分用的那个客户端。
///
/// 只盯 (地址, 钥匙) 这一对，不盯整个 [ConnectionState]：盯整个的话，
/// `account` 一到就会重建客户端并 `close()` 掉它 —— 而记忆浏览器的第一次
/// 检索恰好就在那几百毫秒里飞着。
final apiProvider = Provider<CormexApi>((ref) {
  final (baseUrl, bearer) = ref.watch(
    connectionProvider.select((s) => (s.baseUrl, s.bearer)),
  );
  final api = CormexApi(
    baseUrl: baseUrl,
    token: bearer,
    onUnauthorized: () {
      unawaited(ref.read(connectionProvider.notifier).handleUnauthorized());
    },
  );
  ref.onDispose(api.dispose);
  return api;
});

/// 当前这把凭据对应的是谁。
///
/// # 为什么是一个独立的 provider，而不是 [ConnectionState] 上的一个字段
///
/// 让 [ConnectionController] 自己去问，就得从它里面 `ref.read(apiProvider)`
/// —— 而 `apiProvider` 正 watch 着它。那是一条 connection → api → connection
/// 的环，Riverpod 对这种形状的容忍度取决于读发生在 build 里还是 build 后，
/// 也就是取决于一个将来很容易被人挪动的细节。
///
/// 拆成一个跟着 [apiProvider] 走的 `FutureProvider`，环就不存在了，
/// 而且换钥匙时它自己会重来。
///
/// 失败一律回 null：null 与「这个部署没接账号体系」在界面上是同一件事
/// （没有名字可显示），而顶栏不该因为一次查询失败就变成一条报错 ——
/// 它还挂着「断开」。
final accountProvider = FutureProvider<Account?>((ref) async {
  try {
    return await ref.watch(apiProvider).whoAmI();
  } on Object {
    return null;
  }
});
