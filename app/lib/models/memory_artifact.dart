enum MemoryArtifactKind {
  summary('会话摘要', '按会话整理背景、过程和结果，可随时查看原始对话。');

  const MemoryArtifactKind(this.label, this.description);
  final String label;
  final String description;
}

class MemoryEvidence {
  const MemoryEvidence({
    required this.messageId,
    required this.role,
    required this.quote,
  });
  final String messageId, role, quote;

  factory MemoryEvidence.fromJson(Map<String, dynamic> j) => MemoryEvidence(
    messageId: j['message_id'] as String,
    role: j['role'] as String,
    quote: j['quote'] as String,
  );
}

class MemoryArtifact {
  const MemoryArtifact({
    required this.id,
    required this.kind,
    required this.title,
    required this.markdown,
    required this.sessionId,
    required this.status,
    this.projectName,
    this.chatTitle,
    this.sourceEventIds = const [],
    this.reason,
    this.evidence = const [],
    this.createdAt,
  });

  final String id, title, markdown, sessionId, status;
  final MemoryArtifactKind kind;
  final String? projectName, chatTitle, reason;
  final List<String> sourceEventIds;
  final List<MemoryEvidence> evidence;
  final DateTime? createdAt;

  factory MemoryArtifact.fromJson(Map<String, dynamic> j) => MemoryArtifact(
    id: j['id'] as String,
    kind: MemoryArtifactKind.values.byName(j['kind'] as String),
    title: j['title'] as String? ?? '',
    markdown: j['markdown'] as String? ?? '',
    sessionId: j['session_id'] as String,
    status: j['status'] as String? ?? '',
    projectName: j['project_name'] as String?,
    chatTitle: j['chat_title'] as String?,
    sourceEventIds: (j['source_event_ids'] as List? ?? [])
        .whereType<String>()
        .toList(growable: false),
    reason: j['reason'] as String?,
    evidence: (j['evidence'] as List? ?? [])
        .whereType<Map>()
        .map((e) => MemoryEvidence.fromJson(e.cast<String, dynamic>()))
        .toList(growable: false),
    createdAt: DateTime.tryParse(j['created_at'] as String? ?? '')?.toLocal(),
  );

  String get displayTitle => title.trim().isEmpty ? kind.label : title;
  String get statusLabel => switch (status) {
    'active' => '有效',
    'summary' => '会话摘要',
    'completed' => '已完成',
    'expired' => '已过期',
    'archived' => '已归档',
    '' => '未记录',
    _ => status,
  };
}

class MemoryArtifactsPage {
  const MemoryArtifactsPage({required this.items, this.nextCursor});
  final List<MemoryArtifact> items;
  final String? nextCursor;

  factory MemoryArtifactsPage.fromJson(Map<String, dynamic> j) =>
      MemoryArtifactsPage(
        items: (j['items'] as List? ?? [])
            .whereType<Map>()
            .map((m) => MemoryArtifact.fromJson(m.cast<String, dynamic>()))
            .toList(growable: false),
        nextCursor: switch (j['next_cursor']) {
          final String value when value.trim().isNotEmpty => value,
          _ => null,
        },
      );
}
