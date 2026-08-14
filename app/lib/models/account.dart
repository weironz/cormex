import 'json.dart';

/// 当前这把凭据对应的是谁 —— `GET /auth/me` 的回体。
///
/// 问服务端而不是在登录时把用户名记下来：凭据可以是预共享 token（那时压根
/// 没有登录这一步），也可以是下次启动从凭据库里恢复出来的（那条路上同样
/// 没有用户名）。记在本地的那份会变成一个没人更新的副本。
class Account {
  const Account({
    required this.userId,
    required this.username,
    required this.schemaName,
  });

  final String userId;
  final String username;

  /// 这个人的记忆住在哪个 schema。
  ///
  /// 下发出来是为了排障时能一眼对上：「我看到的是不是我自己的库」在多租户
  /// 里是个会被真的问出口的问题。
  final String schemaName;

  factory Account.fromJson(Map<String, dynamic> json) => Account(
    userId: asString(json['user_id']),
    username: asString(json['username']),
    schemaName: asString(json['schema_name']),
  );

  @override
  bool operator ==(Object other) =>
      other is Account &&
      other.userId == userId &&
      other.username == username &&
      other.schemaName == schemaName;

  @override
  int get hashCode => Object.hash(userId, username, schemaName);
}
