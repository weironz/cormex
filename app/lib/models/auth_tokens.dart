import 'json.dart';

/// 一次登录 / 续期换回来的东西 —— `POST /auth/login` 与 `/auth/refresh`。
///
/// 两种令牌，寿命差两个数量级：
///
/// - [accessToken] 15 分钟，每个请求都带。**不存盘** —— 存一份 15 分钟后就
///   作废的东西没有收益，而每多一处副本就多一处泄露面。
/// - [refreshToken] 30 天，只在续期时用一次，用完就轮转。**它是「关掉应用
///   再打开不用重新登录」的全部依据**，必须存进平台的凭据库。
class AuthTokens {
  const AuthTokens({
    required this.accessToken,
    required this.accessExpiresInSecs,
    required this.refreshToken,
    required this.refreshExpiresInSecs,
    required this.userId,
  });

  final String accessToken;
  final int accessExpiresInSecs;
  final String refreshToken;
  final int refreshExpiresInSecs;
  final String userId;

  /// access token 的本地墙钟到期时刻。
  ///
  /// 由收到的那一刻加上 `expires_in_secs` 算出来，而不是让服务端下发一个
  /// 时间戳：一个时长不需要两端的钟对得上，一个时间戳需要。
  DateTime expiresAt(DateTime now) =>
      now.add(Duration(seconds: accessExpiresInSecs));

  static AuthTokens fromJson(Map<String, Object?> json) => AuthTokens(
    accessToken: asString(json['access_token']),
    accessExpiresInSecs: asInt(json['access_expires_in_secs'], 900),
    refreshToken: asString(json['refresh_token']),
    refreshExpiresInSecs: asInt(json['refresh_expires_in_secs'], 2592000),
    userId: asString(json['user_id']),
  );

  /// 抹掉两个令牌 —— 它俩都是凭据，而 `toString` 会进日志和错误消息。
  @override
  String toString() => 'AuthTokens(user: $userId, <redacted>)';
}
