import 'json.dart';

/// `GET /health` —— 这个部署长什么样。
///
/// 它是**唯一免认证的路由**，所以也是连接页唯一能在拿到凭据之前问的问题。
class HealthStatus {
  const HealthStatus({
    required this.status,
    required this.version,
    required this.role,
    required this.database,
    this.auth = authUnknown,
  });

  final String status;
  final String version;

  /// `"cortexd"`。留着是为了在连上一个**不是** Cormex 的东西时能说清楚：
  /// 一个地址打通了、回的却是别的服务，比连不上更难排查。
  final String role;

  final String database;

  /// `"token"` / `"disabled"`，或者 [authUnknown]（服务端比这个字段还老）。
  ///
  /// 这是 `/health` 上唯一被**用来做决定**而不只是显示的东西：它让连接页
  /// 不去为难一个根本不校验凭据的部署。而这个答案只能住在这里 —— 换到任何
  /// 别的端点上，要问它就得先过那道门。
  final String auth;

  /// 老到不报这个字段的服务端。按「就当它要凭据」处理：猜成 `disabled`
  /// 会把用户送进一个每个请求都 401、又退不回凭据输入框的界面。
  static const String authUnknown = 'unknown';

  bool get requiresToken => auth != 'disabled';

  /// 这个部署谁连上谁就是主人。
  ///
  /// 界面上要**明说**而不是默默享受：cortexd 只有在有人特地写了
  /// `CORTEX_AUTH=disabled` 时才会到这个状态，而它自己每次启动都会打一行
  /// 警告。客户端什么都不说，就成了整个系统里唯一装作没看见的那一环。
  bool get authDisabled => auth == 'disabled';

  bool get isHealthy => status == 'ok';

  /// 存储层没接线时的一句短提示；接好了返回 null。
  ///
  /// 刻意不把它并进 [isHealthy]：开发中的 cortexd 会诚实地报 `not_wired`，
  /// 而那时 `/memory/search` 照样答得出来 —— 把整个界面涂红是过度反应。
  String? get databaseNote => switch (database) {
    'ok' => null,
    'not_wired' => '存储层未接线',
    final other => '存储：$other',
  };

  factory HealthStatus.fromJson(Map<String, dynamic> json) => HealthStatus(
    status: asString(json['status'], 'unknown'),
    version: asString(json['version'], '?'),
    role: asString(json['role'], 'cortexd'),
    database: asString(json['database'], 'unknown'),
    auth: asString(json['auth'], authUnknown),
  );
}
