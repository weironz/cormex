import 'json.dart';

/// 一条被抽取出来的记忆事实 —— `GET /memory/search` 里 `facts` 的一行。
///
/// 这个类是双时间轴的载体：[validAt] 是事件时间（这件事在世界上何时开始为真），
/// [createdAt] 是系统时间（Cormex 何时知道）。回放（`?as_of=`）过滤的是后者。
class MemoryFact {
  const MemoryFact({
    required this.id,
    required this.statement,
    this.predicate,
    this.domain,
    this.confidence,
    this.validAt,
    this.createdAt,
    this.invalidated = false,
    this.sourceEpisodeId,
    this.sourceChannel,
    this.trustTier,
  });

  /// ULID。
  final String id;

  /// 事实的自然语言表述 —— 给人看的那一行。
  final String statement;

  /// 记忆图里的关系名，如 `prefers`、`works_on`。
  final String? predicate;

  /// 粗粒度的话题分桶，如 `coding`、`office`。
  final String? domain;

  /// 抽取器的置信度，落在 `[0, 1]`。
  final double? confidence;

  /// 事件时间：这件事在真实世界何时开始为真。
  final DateTime? validAt;

  /// 系统时间：Cormex 何时知道它。
  final DateTime? createdAt;

  /// 这条事实**此刻**已经被推翻了。
  ///
  /// # 为什么这个字段值得单独存在
  ///
  /// 不带 `as_of` 的检索走 `active_facts` 视图，按定义查不到失效的事实，
  /// 所以那条路上它恒为 false。**它唯一有信息量的场合是回放**：回放故意
  /// 返回「我三个月前以为是真的、后来被推翻了」的行 —— 那正是这个产品的
  /// 卖点，过滤掉等于把回放变成一个只会复述现状的功能。
  ///
  /// 但只返回不标记同样不对：用户会看到一条早已不成立的结论，界面上却和
  /// 现行结论长得一模一样。所以服务端选的是「照样给，但说清楚」，客户端
  /// 这一侧欠的就是把它画出来。
  final bool invalidated;

  /// 出处：这条事实是从哪一条 episode 抽出来的。
  final String? sourceEpisodeId;

  /// 知识从哪条通道进来的：`user_stated` / `conversation` / `derived` /
  /// `tool_output` / `external` / `unknown_legacy`。
  ///
  /// 别跟 [RetrievalChannels] 搞混 —— 那些是**召回**路径（`bm25`、`vector`…）。
  /// 线协议不巧把两件事都叫 channel：这个说的是知识哪来的，那个说的是我们
  /// 怎么又把它找回来的。
  ///
  /// 比这个字段更老的服务端不报它，落到 null；而列存在之前写下的行会以
  /// `unknown_legacy` 这个值到达。
  final String? sourceChannel;

  /// 信任级，1 最高。它与 [sourceChannel] 由数据库的 CHECK 约束绑在一起，
  /// 且单独下发而不是客户端自己推 —— 再推一次就是这份映射的第三份副本，
  /// 而第三份没有任何东西保证它诚实。
  final int? trustTier;

  factory MemoryFact.fromJson(Map<String, dynamic> json) => MemoryFact(
    id: asString(json['id']),
    statement: asString(json['statement']),
    predicate: asStringOrNull(json['predicate']),
    domain: asStringOrNull(json['domain']),
    confidence: asDoubleOrNull(json['confidence']),
    validAt: asDateOrNull(json['valid_at']),
    createdAt: asDateOrNull(json['created_at']),
    invalidated: asBool(json['invalidated']),
    sourceEpisodeId: asStringOrNull(json['source_episode_id']),
    sourceChannel: asStringOrNull(json['source_channel']),
    trustTier: asIntOrNull(json['trust_tier']),
  );

  @override
  bool operator ==(Object other) => other is MemoryFact && other.id == id;

  @override
  int get hashCode => id.hashCode;
}
