import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart' show visibleForTesting;
import 'package:http/http.dart' as http;

import '../models/account.dart';
import '../models/auth_tokens.dart';
import '../models/episode.dart';
import '../models/health_status.dart';
import '../models/json.dart';
import '../models/memory_search_result.dart';
import 'api_exception.dart';
import 'http_client_factory.dart';

/// 对着一台真实的 cortexd 说话。
///
/// # 为什么这里没有 `abstract interface class`
///
/// Cortex 那边有一层抽象接口，因为它有两个实现（真后端 + 一整套 mock
/// fixture）。Cormex 只有一个。给单实现套接口不是解耦，是把每次改动都变成
/// 改两个文件 —— 而那个抽象层在 Cortex 那边有 501 行。
class CormexApi {
  CormexApi({
    required String baseUrl,
    String? token,
    http.Client? client,
    this.onUnauthorized,
  }) : _base = _normalise(baseUrl),
       _token = (token != null && token.trim().isEmpty) ? null : token?.trim(),
       _client = client ?? createHttpClient();

  final Uri _base;
  final http.Client _client;

  /// bearer 凭据。可能是预共享的 daemon token，也可能是登录换来的 access
  /// token —— 服务端的认证中间件两种都认，走的是同一个头。
  ///
  /// 这个实例活着期间不可变：换一把凭据意味着**换一个身份**，而
  /// `apiProvider` 会为此重建整个客户端。做成可写的话，在飞的请求会横跨
  /// 两个身份，闭包里还会留着一把过期的钥匙。
  final String? _token;

  /// 服务端答 401 时敲一次的铃。
  ///
  /// 做成回调而不是「每个调用点自己查 `isUnauthorized`」：过期可能在十来条
  /// 路由里的任何一条上冒出来，而反应永远是同一个（退回连接页）。逐个调用
  /// 点处理，就一定会有漏掉的那一条 —— 而它的症状是一块永远空着的面板，
  /// 或者一个不停转的圈。
  final void Function()? onUnauthorized;

  /// 空地址 = **同源**（在浏览器里），不是「没配」。
  ///
  /// 这两件事在别处栽过：把空串换成一个默认地址，会让「Web 界面与 cortexd
  /// 同源反代」那套拓扑整个走不通 —— 浏览器随即因为 CORS 把响应丢掉，
  /// 报的是「Failed to fetch」，一句看起来像后端没起来的错。
  ///
  /// 判据是「有没有页面源」而不是 `kIsWeb`：`Uri.base` 在浏览器里是当前页面
  /// 的 URL，在桌面端是进程工作目录（`file:` 协议）。桌面端确实没有同源可言，
  /// 那儿的空串才真是「没配」。
  static Uri _normalise(String raw) => resolveBase(raw, Uri.base);

  /// [_normalise] 的纯函数版，`page` 就是 `Uri.base`。
  ///
  /// 提出来只为一件事：**能测同源那一支**。`Uri.base` 在测试进程里恒为
  /// `file:`，直接测 `_normalise` 只能验到桌面端那条回落。
  ///
  /// # 三种写法，两条语义
  ///
  /// | 写进来的 | 意思 |
  /// |---|---|
  /// | `''` | 同源，API 就在页面根上 |
  /// | `/api` | 同源，但 API 挂在一段前缀后面 |
  /// | `memory.example.com/api` | 另一个源 |
  ///
  /// **纯路径那一支是 Web 镜像能开源的原因。** 换成「构建时把
  /// `https://自己的域名/api` 编译进 `main.dart.js`」也能跑通同一套部署，
  /// 但那样发出去的镜像烧着我们的域名，别人拿到手第一件事是重建。
  /// 而路径前缀是同源相对的：谁的域名都对。
  @visibleForTesting
  static Uri resolveBase(String raw, Uri page) {
    var s = raw.trim();

    // 先削尾斜杠。`length > 1` 是为了让 `/` 活到下一行被当成「同源根」——
    // 无条件削会把它变成空串，那正好也是同源根，但绕了一圈且看不出是有意的。
    while (s.length > 1 && s.endsWith('/')) {
      s = s.substring(0, s.length - 1);
    }
    if (s == '/') s = '';

    if (s.isEmpty || s.startsWith('/')) {
      // 同源。没有页面源（桌面端的 `file:`）时回落到本机 daemon，
      // **路径前缀仍然保留** —— 那是「API 挂在哪」，与「哪台机器」无关。
      final root = _pageOrigin(page) ?? Uri.parse('http://127.0.0.1:8080');
      return s.isEmpty ? root : root.replace(path: s);
    }

    if (!s.startsWith('http://') && !s.startsWith('https://')) {
      s = 'http://$s';
    }
    return Uri.parse(s);
  }

  /// 这一端能不能靠同源解析出地址 —— 也就是「连接页要不要问地址」。
  ///
  /// 浏览器里是能的（页面自己就在某个源上），桌面端不能：那儿的 `Uri.base`
  /// 是进程工作目录（`file:`），没有源可言，不问就只能猜本机 8080。
  ///
  /// **判据是「有没有页面源」而不是 `kIsWeb`** —— 与 [resolveBase] 同一条，
  /// 两处要是各判各的，就会出现「地址栏藏起来了，但解析回落到了 localhost」
  /// 这种页面上完全看不出来的错位。
  static bool get canUseSameOrigin => _pageOrigin(Uri.base) != null;

  /// 页面所在的源，只留 scheme + host + port；不在浏览器里时为 null。
  ///
  /// **重新构造，不能用 `page.replace(query: null)`** —— `Uri.replace` 里的
  /// null 是「这一项不动」而不是「清空」，于是 `…/app/?x=1#frag` 的查询串会
  /// 原样留在 API 根上，之后每个请求都带着它。
  static Uri? _pageOrigin(Uri page) {
    if (page.scheme != 'http' && page.scheme != 'https') return null;
    return Uri(
      scheme: page.scheme,
      host: page.host,
      port: page.hasPort ? page.port : null,
    );
  }

  Uri _uri(String path, [Map<String, String>? query]) => _base.replace(
    path: '${_base.path}$path',
    queryParameters: (query == null || query.isEmpty) ? null : query,
  );

  String get label => '$_base · $kHttpClientKind';

  Uri get baseUri => _base;

  Map<String, String> _headers([Map<String, String> extra = const {}]) => {
    ...extra,
    if (_token case final token?) 'authorization': 'Bearer $token',
  };

  /// 给 >= 400 的响应造异常，顺便在 401 上敲铃。
  ///
  /// 收在一处，好让哪条路由都忘不掉：[onUnauthorized] 的全部意义就是
  /// 「过期在哪儿发生都会被发现」，而不是「在有人记得加标注的那几条上」。
  CormexApiException _failure(int status, String message) {
    if (status == 401 && onUnauthorized != null) {
      // 推迟一个微任务：监听器会丢掉凭据，那会重建 provider 并 dispose 掉
      // **当前这个**实例 —— 包括我们此刻手上还攥着响应的那个 HTTP client。
      scheduleMicrotask(onUnauthorized!);
    }
    return CormexApiException(message, statusCode: status);
  }

  // ─────────────────────────────── 探测 ───────────────────────────────

  /// `GET /health` —— **唯一**免认证的路由。
  ///
  /// 正因为免认证，它才能当连接页的探针：它会对一个还没有凭据的客户端
  /// 回答 `auth: "token" | "disabled"`，而那恰恰是这种客户端在决定要不要
  /// 向用户索要凭据之前唯一需要知道的事。
  Future<HealthStatus> health() async =>
      HealthStatus.fromJson(await _getJson('/health'));

  /// `POST /auth/ticket` —— 拿长期凭据换一张 60 秒的短命票据。
  ///
  /// 这个应用**不用**那张票（它不开 WebSocket，也不用 `<img src>` 取附件）。
  /// 留着它是因为它是**最便宜的一次「这把钥匙认不认」探测**：它需要认证、
  /// 服务端只查一次摘要、回体是常数大小。
  ///
  /// 换成拿 `/memory/search` 探测的话，一次失败的连接尝试会去跑四路召回；
  /// 换成 `/auth/me` 的话，404 既可能是「没接账号体系」也可能是「路由不在」，
  /// 探测不出凭据对不对。
  Future<void> verifyCredential() async {
    final http.Response response;
    try {
      response = await _client.post(
        _uri('/auth/ticket'),
        headers: _headers(const {'accept': 'application/json'}),
      );
    } on Object catch (e) {
      throw CormexApiException(_unreachableMessage(e), cause: e);
    }
    if (response.statusCode >= 400) {
      throw _failure(response.statusCode, _errorMessage(response));
    }
  }

  /// `GET /auth/me` —— 当前这把凭据对应的是谁。
  ///
  /// 返回 null 有两种意思，**对调用方是同一种**（没有名字可显示）：这个
  /// 部署没接账号体系（预共享 token 的自托管），或者服务端老到没有这条
  /// 路由。两种都不该让顶栏消失。
  Future<Account?> whoAmI() async {
    try {
      return Account.fromJson(await _getJson('/auth/me'));
    } on CormexApiException catch (e) {
      if (e.isMissing || e.statusCode == 501) return null;
      rethrow;
    }
  }

  // ─────────────────────────────── 记忆 ───────────────────────────────

  /// `GET /memory/search?q=&limit=&as_of=`
  ///
  /// [asOf] 按**系统时间**回放：返回那一刻**已知**的记忆，而不是那一刻
  /// **为真**的记忆 —— 这是双时间轴里「我三个月前以为什么」那一根轴。
  /// null 就是「现在」。
  ///
  /// [query] 留空是合法的，且意思不同：服务端那时给的是那一刻的**全貌**
  /// （按时间倒序），而不是一次空检索。
  Future<MemorySearchResult> searchMemory(
    String query, {
    int limit = 30,
    DateTime? asOf,
  }) async {
    final json = await _getJson('/memory/search', {
      'q': query,
      'limit': '$limit',
      // RFC 3339 UTC —— 服务端拿它跟系统时间比。
      if (asOf != null) 'as_of': asOf.toUtc().toIso8601String(),
    });
    return MemorySearchResult.fromJson(json);
  }

  /// `GET /episodes/{id}` —— 一条事实的出处。
  Future<Episode> episode(String id) async =>
      Episode.fromJson(await _getJson('/episodes/${Uri.encodeComponent(id)}'));

  // ─────────────────────────────── 账号 ───────────────────────────────

  /// `POST /auth/login` —— 用户名 + 密码换一对令牌。
  ///
  /// 拿回来的 refresh token 是**「下次打开不用再登录」的全部依据**，
  /// 调用方必须把它存进平台的凭据库（见 `lib/auth/`）。
  Future<AuthTokens> login(String username, String password) =>
      _postAuth('/auth/login', {'username': username, 'password': password});

  /// `POST /auth/refresh` —— 用长效凭据换一对新的。
  ///
  /// 每次都会**轮转**：服务端把旧的作废并签一个新的。所以调用方必须存下
  /// 新的那个 —— 继续用旧的会被判成重放，而重放会让整条链一起失效。
  Future<AuthTokens> refreshSession(String refreshToken) =>
      _postAuth('/auth/refresh', {'refresh_token': refreshToken});

  /// `POST /auth/logout` —— 让服务端作废这条链。
  ///
  /// 与「本地删掉副本」不是一回事：本地删除只让这台机器忘了，而已经泄露
  /// 出去的那一份照样能用到 30 天后。
  Future<void> logout(String refreshToken) async {
    try {
      await _client.post(
        _uri('/auth/logout'),
        headers: _headers(const {'content-type': 'application/json'}),
        body: jsonEncode({'refresh_token': refreshToken}),
      );
    } on Object catch (_) {
      // 登出失败不该拦住用户离开。本地那份已经会被清掉，而服务端那条链
      // 最多再活到过期 —— 把人卡在「退不出去」的界面上更糟。
    }
  }

  /// 三条账号端点形状一样：POST JSON，回 AuthTokens。
  ///
  /// **不带 Authorization 头**：它们在服务端是公开端点（登录时本来就还
  /// 没有凭据），带一个过期的 access token 反而可能被中间层拦下。
  Future<AuthTokens> _postAuth(String path, Map<String, Object?> body) async {
    final http.Response response;
    try {
      response = await _client.post(
        _uri(path),
        headers: const {
          'content-type': 'application/json',
          'accept': 'application/json',
        },
        body: jsonEncode(body),
      );
    } on Object catch (e) {
      throw CormexApiException(_unreachableMessage(e), cause: e);
    }
    if (response.statusCode >= 400) {
      throw _failure(response.statusCode, _errorMessage(response));
    }
    return AuthTokens.fromJson(
      _decodeObject(path, utf8.decode(response.bodyBytes)),
    );
  }

  // ─────────────────────────────── 管道 ───────────────────────────────

  Future<Map<String, dynamic>> _getJson(
    String path, [
    Map<String, String>? query,
  ]) async {
    final http.Response response;
    try {
      response = await _client.get(
        _uri(path, query),
        headers: _headers(const {'accept': 'application/json'}),
      );
    } on Object catch (e) {
      throw CormexApiException(_unreachableMessage(e), cause: e);
    }
    if (response.statusCode >= 400) {
      throw _failure(response.statusCode, _errorMessage(response));
    }
    return _decodeObject(path, utf8.decode(response.bodyBytes));
  }

  String _unreachableMessage(Object e) =>
      '连不上 cortexd（$_base）。确认它已启动、地址与端口正确，'
      '并且允许了这个页面的跨源请求。\n$e';

  /// 从错误响应里挖出服务端写给人看的那句话。
  ///
  /// cortexd 的错误体是 `{"error": "…"}`，而那句话是专门措辞给用户看的
  /// （比如路径校验的拒绝理由）。挖不出来就退回原始 body —— 编一句更友好的
  /// 通用文案，只会盖掉唯一有信息量的东西。
  String _errorMessage(http.Response response) {
    final body = utf8.decode(response.bodyBytes);
    if (body.isEmpty) return 'HTTP ${response.statusCode}';
    try {
      final decoded = jsonDecode(body);
      if (decoded is Map<String, dynamic>) {
        final msg = asStringOrNull(decoded['error']);
        if (msg != null) return msg;
      }
    } on FormatException {
      // 不是 JSON。往下走，把原文给出去。
    }
    return _trim(body);
  }

  /// 解一个 JSON 对象体，并把「这压根不是 JSON」变成一句指得出病因的话。
  ///
  /// `jsonDecode` 撞上 HTML 体时抛的是
  /// `FormatException: Unexpected character (at character 1) <!DOCTYPE html>`。
  /// 那句话技术上完全正确，而且**是首次连接最可能撞上的失败**：最顺手输入
  /// 的地址是浏览器里访问的那个，而 API 常常在深一层路径上
  /// （`https://host` → Web 界面，`https://host/api` → cortexd）。
  Map<String, dynamic> _decodeObject(String what, String body) {
    final Object? decoded;
    try {
      decoded = jsonDecode(body);
    } on FormatException {
      throw CormexApiException(_notJsonMessage(what, body));
    }
    if (decoded is! Map<String, dynamic>) {
      throw CormexApiException('$what 返回了非对象 JSON');
    }
    return decoded;
  }

  String _notJsonMessage(String what, String body) {
    final head = body.trimLeft();
    if (head.startsWith('<')) {
      // 地址上没有路径，就是典型的「我把浏览器里的网址填进来了」；
      // 直接建议一个具体地址，而不是描述问题的形状。已经带了路径的话，
      // 再猜就比没用还糟 —— 说出观察到的事实然后打住。
      final bare = _base.path.replaceAll('/', '').isEmpty;
      final hint = bare
          ? '自托管部署通常把 cortexd 挂在 /api 下 —— 试试 $_base/api。'
          : '确认这个地址指向 cortexd 本身，而不是 Web 界面或反代的默认站点。';
      return '$what：这个地址返回的是网页，不是 API。$hint';
    }
    return '$what：返回的不是 JSON。\n${_trim(body)}';
  }

  static String _trim(String s) =>
      s.length <= 400 ? s : '${s.substring(0, 400)}…';

  void dispose() => _client.close();
}
