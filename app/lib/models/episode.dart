import 'json.dart';

/// 一条只增不改的归档对话轮次 —— 每条记忆事实的出处所指
/// （`GET /episodes/{id}`）。
///
/// # 这个类是从 Cortex 那边**拆**过来的，不是搬过来的
///
/// Cortex 的 `Episode` 还带着 `attachments`、`memories`、`tool_calls` 三段，
/// 于是它一个类要拖进 `Attachment` / `InjectedMemory` / `ToolCall` 三个模型
/// —— 而那三样全是 agent 那一侧的东西：附件是给模型读的 blob，注入记忆是
/// 「为什么记得这个」抽屉，工具调用只有会话里才有。
///
/// 服务端照旧下发它们（`EpisodeDto` 是两个产品共用的线协议），这里**不解析**。
/// 少解析一个字段的代价是零，多拖三个模型进来的代价是这个应用得连着 agent
/// 的一半概念一起编 —— 而 Cormex 里根本没有 agent。
class Episode {
  const Episode({
    required this.id,
    required this.sessionId,
    required this.role,
    required this.text,
    this.occurredAt,
  });

  final String id;
  final String sessionId;

  /// `user` | `assistant` | `system`。保持字符串：这套词表归服务端所有，
  /// 它可能会长，而这里只负责显示。
  final String role;

  /// 归档的纯文本。
  ///
  /// null 是**合法值**，意思是这一轮没有可显示的纯文本（比如整轮只有工具
  /// 调用或附件）。渲染成空白框会让人以为数据丢了，所以界面上要说出来。
  final String? text;

  final DateTime? occurredAt;

  factory Episode.fromJson(Map<String, dynamic> json) => Episode(
    id: asString(json['id']),
    sessionId: asString(json['session_id']),
    role: asString(json['role'], 'user'),
    text: asStringOrNull(json['text']),
    occurredAt: asDateOrNull(json['occurred_at']),
  );
}
