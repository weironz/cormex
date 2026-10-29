import 'json.dart';

/// 一条归一化事件的类型。
///
/// 与服务端 `cormex_proto::transcript::TranscriptKind` 的拼写一一对应。
enum TranscriptKind {
  userMessage('user_message'),
  agentMessage('agent_message'),
  agentThought('agent_thought'),
  toolCall('tool_call'),
  toolResult('tool_result'),
  plan('plan'),
  system('system'),
  meta('meta'),
  unknown('unknown');

  const TranscriptKind(this.wire);

  final String wire;

  /// 认不出的类型落到 [unknown] 而不是抛异常：界面要能对着**比它新**的
  /// 服务端活下来，而一个新的事件类型不该让整段会话显示不出来。
  static TranscriptKind fromWire(String? s) => values.firstWhere(
    (k) => k.wire == s,
    orElse: () => TranscriptKind.unknown,
  );
}

/// 内容块。与 ACP 对齐，`type` 是判别字段。
sealed class ContentBlock {
  const ContentBlock();

  static ContentBlock fromJson(Map<String, dynamic> json) =>
      switch (asString(json['type'])) {
        'text' => TextBlock(asString(json['text'])),
        'thought' => ThoughtBlock(asString(json['text'])),
        'tool_call' => ToolCallBlock(
          toolCallId: asString(json['tool_call_id']),
          name: asString(json['name']),
          input: json['input'],
        ),
        'tool_result' => ToolResultBlock(
          toolCallId: asString(json['tool_call_id']),
          output: asString(json['output']),
          isError: json['is_error'] == true,
        ),
        // `new` is a Dart keyword, so the field is `newContent`; the wire key
        // stays `new`
        'diff' => DiffBlock(
          path: asString(json['path']),
          oldContent: asString(json['old']),
          newContent: asString(json['new']),
        ),
        'plan' => PlanBlock(switch (json['entries']) {
          final List<dynamic> l => l.map(asString).toList(),
          _ => const [],
        }),
        'image' => ImageBlock(asString(json['blob_ref'])),
        'resource_ref' => ResourceRefBlock(
          generationId: asString(json['generation_id']),
          offset: asInt(json['offset']),
          length: asInt(json['length']),
          mime: asString(json['mime']),
        ),
        final other => UnknownBlock(other.isEmpty ? 'unknown' : other),
      };
}

class TextBlock extends ContentBlock {
  const TextBlock(this.text);
  final String text;
}

class ThoughtBlock extends ContentBlock {
  const ThoughtBlock(this.text);
  final String text;
}

class ToolCallBlock extends ContentBlock {
  const ToolCallBlock({
    required this.toolCallId,
    required this.name,
    this.input,
  });
  final String toolCallId;
  final String name;

  /// 宿主记录的原始参数。保持 JSON 而不是字符串：不同工具的展示方式不同，
  /// 转成字符串会逼界面再解析一次。
  final Object? input;

  /// 一行摘要，给折叠后的工具调用用。
  String get summary {
    final i = input;
    if (i is Map<String, dynamic>) {
      for (final key in ['command', 'file_path', 'path', 'pattern', 'query']) {
        final v = i[key];
        if (v is String && v.isNotEmpty) return v;
      }
    }
    return name;
  }
}

class ToolResultBlock extends ContentBlock {
  const ToolResultBlock({
    required this.toolCallId,
    required this.output,
    this.isError = false,
  });
  final String toolCallId;
  final String output;
  final bool isError;
}

class DiffBlock extends ContentBlock {
  const DiffBlock({
    required this.path,
    required this.oldContent,
    required this.newContent,
  });
  final String path;
  final String oldContent;
  final String newContent;
}

class PlanBlock extends ContentBlock {
  const PlanBlock(this.entries);
  final List<String> entries;
}

class ImageBlock extends ContentBlock {
  const ImageBlock(this.blobRef);
  final String blobRef;
}

/// 太大而没有内联的块：指回 R 层的引用。
class ResourceRefBlock extends ContentBlock {
  const ResourceRefBlock({
    required this.generationId,
    required this.offset,
    required this.length,
    required this.mime,
  });
  final String generationId;
  final int offset;
  final int length;
  final String mime;
}

class UnknownBlock extends ContentBlock {
  const UnknownBlock(this.rawType);
  final String rawType;
}

/// 一条归一化事件。
class TranscriptEvent {
  const TranscriptEvent({
    required this.id,
    required this.seq,
    required this.kind,
    this.occurredAt,
    this.role,
    this.content = const [],
    this.text,
    this.toolName,
    this.toolCallId,
    this.sourceGenerationId,
    this.sourceOffset = 0,
    this.sourceLength = 0,
  });

  final String id;
  final int seq;
  final TranscriptKind kind;
  final DateTime? occurredAt;
  final String? role;
  final List<ContentBlock> content;
  final String? text;
  final String? toolName;
  final String? toolCallId;

  /// 这条事件来自哪一代的哪个字节区间 —— 「凭什么这么认为」的最后一跳。
  final String? sourceGenerationId;
  final int sourceOffset;
  final int sourceLength;

  /// 纯文本，供展示。
  String get plainText {
    final t = text;
    if (t != null && t.isNotEmpty) return t;
    return content
        .whereType<TextBlock>()
        .map((b) => b.text)
        .join('\n');
  }

  factory TranscriptEvent.fromJson(Map<String, dynamic> json) {
    final source = switch (json['source']) {
      final Map<String, dynamic> m => m,
      _ => const <String, dynamic>{},
    };
    return TranscriptEvent(
      id: asString(json['id']),
      seq: asInt(json['seq']),
      kind: TranscriptKind.fromWire(asStringOrNull(json['kind'])),
      occurredAt: switch (asStringOrNull(json['occurred_at'])) {
        final s => s == null ? null : DateTime.tryParse(s)?.toLocal(),
      },
      role: asStringOrNull(json['role']),
      content: switch (json['content']) {
        final List<dynamic> l => l
            .whereType<Map<String, dynamic>>()
            .map(ContentBlock.fromJson)
            .toList(),
        _ => const [],
      },
      text: asStringOrNull(json['text']),
      toolName: asStringOrNull(json['tool_name']),
      toolCallId: asStringOrNull(json['tool_call_id']),
      sourceGenerationId: asStringOrNull(source['generation_id']),
      sourceOffset: asInt(source['offset']),
      sourceLength: asInt(source['length']),
    );
  }
}

/// `GET /archive/sessions/{id}/events` 的响应。
class TranscriptEventsPage {
  const TranscriptEventsPage({this.events = const [], this.nextCursor});

  final List<TranscriptEvent> events;
  final String? nextCursor;

  factory TranscriptEventsPage.fromJson(Map<String, dynamic> json) =>
      TranscriptEventsPage(
        events: switch (json['events']) {
          final List<dynamic> l => l
              .whereType<Map<String, dynamic>>()
              .map(TranscriptEvent.fromJson)
              .toList(),
          _ => const [],
        },
        nextCursor: asStringOrNull(json['next_cursor']),
      );
}
