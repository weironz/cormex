import 'json.dart';

class ArchiveProject {
  const ArchiveProject({this.key, this.name, required this.chatCount});
  final String? key;
  final String? name;
  final int chatCount;

  String get displayName {
    if (key == null) return '未分类';
    final label = name?.trim();
    if (label != null && label.isNotEmpty) return label;
    final path = key!
        .replaceFirst(RegExp(r'^local:'), '')
        .replaceAll('\\', '/');
    final segments = path.split('/').where((part) => part.isNotEmpty);
    return segments.isEmpty ? path : segments.last;
  }

  factory ArchiveProject.fromJson(Map<String, dynamic> json) => ArchiveProject(
    key: asStringOrNull(json['key']),
    name: asStringOrNull(json['name']),
    chatCount: asInt(json['chat_count']),
  );
}

/// 有会话的一个来源，给来源筛选用（`GET /archive/sources`）。
///
/// 名字、厂商、形态都来自服务端的采集目录，和 CLI 采集时用的是同一份。
class ArchiveSource {
  const ArchiveSource({
    required this.agent,
    required this.name,
    this.provider,
    this.kind,
    required this.chatCount,
  });

  final String agent;
  final String name;
  final String? provider;

  /// `chat` / `code`。
  final String? kind;
  final int chatCount;

  factory ArchiveSource.fromJson(Map<String, dynamic> json) => ArchiveSource(
    agent: asString(json['agent']),
    name: asString(json['name'], asString(json['agent'])),
    provider: asStringOrNull(json['provider']),
    kind: asStringOrNull(json['kind']),
    chatCount: asInt(json['chat_count']),
  );
}

/// 一次导入的结果（`/archive/import/preview` 与 `/run`）。
///
/// `newCount + changed + unchanged == conversations`。同一份文件导第二次，
/// 全部是 unchanged，什么都不写。
class ArchiveImportSummary {
  const ArchiveImportSummary({
    required this.agent,
    required this.source,
    required this.conversations,
    required this.newCount,
    required this.changed,
    required this.unchanged,
    required this.written,
  });

  final String agent;
  final String source;
  final int conversations;
  final int newCount;
  final int changed;
  final int unchanged;
  final bool written;

  /// 这次（会）写入的对话数。
  int get toWrite => newCount + changed;

  factory ArchiveImportSummary.fromJson(Map<String, dynamic> json) =>
      ArchiveImportSummary(
        agent: asString(json['agent']),
        source: asString(json['source'], asString(json['agent'])),
        conversations: asInt(json['conversations']),
        newCount: asInt(json['new']),
        changed: asInt(json['changed']),
        unchanged: asInt(json['unchanged']),
        written: json['written'] == true,
      );
}

/// 档案里的一段会话。
///
/// 与服务端 `cormex_proto::archive::ArchiveSessionDto` 一一对应。
class ArchiveSession {
  const ArchiveSession({
    required this.id,
    required this.agent,
    this.title,
    this.projectKey,
    this.projectName,
    this.pinnedPosition,
    this.sourceArchived = false,
    this.devices = const [],
    this.firstAt,
    this.lastAt,
    this.eventCount = 0,
    this.messageCount,
    this.summary,
    this.soleCopy = false,
  });

  final String id;
  final String agent;

  /// 第一条用户消息的开头。阶段 5 有摘要之后由摘要取代。
  final String? title;

  final String? projectKey;
  final String? projectName;
  final int? pinnedPosition;
  final bool sourceArchived;

  /// 给这段会话贡献过文件的设备。
  final List<String> devices;

  final DateTime? firstAt;
  final DateTime? lastAt;
  final int eventCount;
  final int? messageCount;

  /// 一行摘要。阶段 5 之前为 null。
  final String? summary;

  /// **Cormex 手里是唯一一份。**
  ///
  /// 宿主在 `cleanupPeriodDays` 之后会删掉自己的会话，那时这里就是最后一份
  /// —— 界面必须在删除按钮之前说清这件事，而不是之后。
  final bool soleCopy;

  /// 列表里显示的名字：标题，没有就用 agent 名。
  String get displayTitle {
    final t = title?.trim();
    if (t != null && t.isNotEmpty) return t;
    return agent;
  }

  factory ArchiveSession.fromJson(Map<String, dynamic> json) => ArchiveSession(
    id: asString(json['id']),
    agent: asString(json['agent'], 'unknown'),
    title: asStringOrNull(json['title']),
    projectKey: asStringOrNull(json['project_key']),
    projectName: asStringOrNull(json['project_name']),
    pinnedPosition: json['pinned_position'] is int
        ? json['pinned_position'] as int
        : null,
    sourceArchived: json['source_archived'] == true,
    devices: switch (json['devices']) {
      final List<dynamic> l =>
        l.map(asString).where((s) => s.isNotEmpty).toList(),
      _ => const [],
    },
    firstAt: _time(json['first_at']),
    lastAt: _time(json['last_at']),
    eventCount: asInt(json['event_count']),
    messageCount: json['message_count'] is num
        ? (json['message_count'] as num).toInt()
        : null,
    summary: asStringOrNull(json['summary']),
    soleCopy: json['sole_copy'] == true,
  );

  static DateTime? _time(Object? v) {
    final s = asStringOrNull(v);
    return s == null ? null : DateTime.tryParse(s)?.toLocal();
  }
}

/// `GET /archive/sessions` 的响应。
class ArchiveSessionsPage {
  const ArchiveSessionsPage({this.sessions = const [], this.nextCursor});

  final List<ArchiveSession> sessions;

  /// 下一页的游标。null = 这是最后一页。
  final String? nextCursor;

  factory ArchiveSessionsPage.fromJson(Map<String, dynamic> json) =>
      ArchiveSessionsPage(
        sessions: switch (json['sessions']) {
          final List<dynamic> l =>
            l
                .whereType<Map<String, dynamic>>()
                .map(ArchiveSession.fromJson)
                .toList(),
          _ => const [],
        },
        nextCursor: asStringOrNull(json['next_cursor']),
      );
}

/// 会话里的一个文件，带它的代。
class ArchiveFile {
  const ArchiveFile({
    required this.id,
    required this.role,
    required this.deviceId,
    required this.logicalPath,
    this.generations = const [],
  });

  final String id;
  final String role;
  final String deviceId;

  /// 相对 agent home 的路径，正斜杠。
  final String logicalPath;

  final List<ArchiveGeneration> generations;

  factory ArchiveFile.fromJson(Map<String, dynamic> json) => ArchiveFile(
    id: asString(json['id']),
    role: asString(json['role']),
    deviceId: asString(json['device_id']),
    logicalPath: asString(json['logical_path']),
    generations: switch (json['generations']) {
      final List<dynamic> l =>
        l
            .whereType<Map<String, dynamic>>()
            .map(ArchiveGeneration.fromJson)
            .toList(),
      _ => const [],
    },
  );
}

/// 文件的一代。追加不开新代；改写 / 截断开新代，旧代原样保留。
class ArchiveGeneration {
  const ArchiveGeneration({
    required this.id,
    required this.ordinal,
    required this.reason,
    this.endOffset = 0,
    this.createdAt,
  });

  final String id;
  final int ordinal;

  /// `initial` / `rewrite` / `truncate` / `snapshot`
  final String reason;

  final int endOffset;
  final DateTime? createdAt;

  factory ArchiveGeneration.fromJson(Map<String, dynamic> json) =>
      ArchiveGeneration(
        id: asString(json['id']),
        ordinal: asInt(json['ordinal']),
        reason: asString(json['reason']),
        endOffset: asInt(json['end_offset']),
        createdAt: ArchiveSession._time(json['created_at']),
      );
}

/// `GET /archive/sessions/{id}` 的响应。
class ArchiveSessionDetail {
  const ArchiveSessionDetail({
    required this.session,
    this.files = const [],
    this.parserVersion,
    this.projectionStatus,
    this.projectionError,
  });

  final ArchiveSession session;
  final List<ArchiveFile> files;
  final String? parserVersion;

  /// `ok` / `partial` / `failed`。
  ///
  /// `failed` 时界面要说明原因，而不是显示一段空对话 —— 空对话读起来像
  /// 「这段会话没内容」，而实际是「解析失败了」。
  final String? projectionStatus;
  final String? projectionError;

  factory ArchiveSessionDetail.fromJson(Map<String, dynamic> json) =>
      ArchiveSessionDetail(
        session: switch (json['session']) {
          final Map<String, dynamic> m => ArchiveSession.fromJson(m),
          _ => const ArchiveSession(id: '', agent: 'unknown'),
        },
        files: switch (json['files']) {
          final List<dynamic> l =>
            l
                .whereType<Map<String, dynamic>>()
                .map(ArchiveFile.fromJson)
                .toList(),
          _ => const [],
        },
        parserVersion: asStringOrNull(json['parser_version']),
        projectionStatus: asStringOrNull(json['projection_status']),
        projectionError: asStringOrNull(json['projection_error']),
      );
}
