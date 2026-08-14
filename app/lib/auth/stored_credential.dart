import 'dart:convert';

/// 存下来的那把钥匙**是哪一种**。
///
/// 必须记住，因为两者的恢复路径完全不同：bearer 拿出来直接用，refresh
/// 必须先去换一对新的。只存字符串的话，启动时就得靠「先当 bearer 试一次，
/// 401 了再当 refresh 试」来猜 —— 而猜错的那一半会在服务端留下一条失败的
/// 认证记录，还白白多一次往返。
enum CredentialKind {
  /// 预共享的 daemon token，或者关掉认证的部署里那个「没有钥匙」。
  /// 直接当 `Authorization: Bearer` 用，不过期。
  bearer,

  /// 登录换来的长效令牌。**不能直接当请求凭据用** —— 它只能去
  /// `POST /auth/refresh` 换一个 15 分钟的 access token。
  refresh,
}

/// 一把存进凭据库的钥匙。
class StoredCredential {
  const StoredCredential({required this.kind, required this.value});

  final CredentialKind kind;
  final String value;

  /// 序列化成一行，好整体塞进凭据库的一个键里。
  ///
  /// 存成 JSON 而不是两个键：两个键会有「只写进去一个」的中间态，而恢复时
  /// 读到一个没有 kind 的 value 只能回到上面说的那种猜。
  String encode() => jsonEncode({'kind': kind.name, 'value': value});

  /// 解不出来一律当作「没存过」。
  ///
  /// 存坏了的条目、上一个版本写的格式、被别的软件覆盖掉的键 —— 都落在这里，
  /// 而它们的意思都一样：得重新连一次。抛异常只会让应用起不来。
  static StoredCredential? decode(String? raw) {
    if (raw == null || raw.isEmpty) return null;
    try {
      final json = jsonDecode(raw);
      if (json is! Map<String, dynamic>) return null;
      final value = json['value'];
      if (value is! String || value.isEmpty) return null;
      final kind = CredentialKind.values.firstWhere(
        (k) => k.name == json['kind'],
        orElse: () => CredentialKind.bearer,
      );
      return StoredCredential(kind: kind, value: value);
    } on FormatException {
      return null;
    }
  }

  /// 抹掉值 —— 它是凭据，而 `toString` 会进日志与错误消息。
  @override
  String toString() => 'StoredCredential(${kind.name}, <redacted>)';
}
