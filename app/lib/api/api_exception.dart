/// [CormexApi] 抛出的每一种失败都归一成这一个类型，好让界面永远不必去
/// 匹配平台相关的 socket / JS 异常（`dart:io` 与浏览器 fetch 那两套长得
/// 完全不一样）。
class CormexApiException implements Exception {
  const CormexApiException(this.message, {this.statusCode, this.cause});

  final String message;
  final int? statusCode;
  final Object? cause;

  /// 请求压根没到服务端（daemon 没起、端口不对、CORS 被拒）。
  /// 值得在界面上单独说一句 —— 它的下一步是去看后端，不是去看凭据。
  bool get isUnreachable => statusCode == null;

  /// 服务端听懂了，然后回答「我没有这个」。
  ///
  /// 与传输失败分开，因为两者的正确反应相反：连不上应该**重试**，
  /// 不支持**不该**重试。404 也算 —— 一条从没注册过的路由回的就是 404，
  /// 而从客户端这一侧看，它跟显式的 501 无法区分、补救办法也一样。
  bool get isUnsupported =>
      statusCode == 501 || statusCode == 405 || statusCode == 404;

  /// 就是 404 ——「你点名要的那个东西不在」。
  ///
  /// 每一条按 id 取单个资源的端点都有 404 的这第二种读法，而它与
  /// [isUnsupported] 撞车。取 episode 正是这种：一个被 purge 掉的出处
  /// 与一个没有 `/episodes` 路由的服务端，回的是同一个码。
  bool get isMissing => statusCode == 404;

  /// 服务端要凭据而我们没给，或者给的那把被拒了。
  ///
  /// `cormexd::auth::require` 故意不区分「没带」与「带错」（分开报等于给
  /// 暴力破解一个进度条），所以这里也不区分。客户端的反应两种情况一样：
  /// 退回连接页，而不是摆一块空面板或者转一个永远不停的圈。
  bool get isUnauthorized => statusCode == 401;

  @override
  String toString() =>
      statusCode == null ? message : 'HTTP $statusCode · $message';
}
