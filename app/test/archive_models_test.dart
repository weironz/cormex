import 'package:cormex_app/models/archive_session.dart';
import 'package:cormex_app/models/transcript_event.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('ArchiveSession', () {
    test('解析服务端返回的会话行', () {
      final s = ArchiveSession.fromJson({
        'id': '01S',
        'agent': 'claude_code',
        'title': '对象存储用什么',
        'project_key': 'github.com/u/p',
        'devices': ['01D1', '01D2'],
        'first_at': '2026-10-29T08:00:00Z',
        'last_at': '2026-10-29T09:00:00Z',
        'event_count': 42,
        'sole_copy': true,
      });
      expect(s.id, '01S');
      expect(s.title, '对象存储用什么');
      expect(s.devices, ['01D1', '01D2']);
      expect(s.eventCount, 42);
      expect(
        s.soleCopy,
        isTrue,
        reason: '宿主已经删掉原始文件时界面必须知道 —— 在这里删除就是删掉最后一份',
      );
      expect(s.lastAt, isNotNull);
    });

    test('缺字段不抛异常', () {
      // 界面要能对着比它新、比它旧的服务端都活下来
      final s = ArchiveSession.fromJson({'id': '01S'});
      expect(s.agent, 'unknown');
      expect(s.title, isNull);
      expect(s.devices, isEmpty);
      expect(s.soleCopy, isFalse);
    });

    test('没有标题时退回 agent 名', () {
      const s = ArchiveSession(id: '01S', agent: 'codex');
      expect(s.displayTitle, 'codex');
    });

    test('空白标题也退回 agent 名', () {
      const s = ArchiveSession(id: '01S', agent: 'codex', title: '   ');
      expect(
        s.displayTitle,
        'codex',
        reason: '一个全是空格的标题在列表里是一行空白，读起来像界面坏了',
      );
    });
  });

  group('TranscriptKind', () {
    test('已知类型按线上拼写解析', () {
      expect(TranscriptKind.fromWire('user_message'), TranscriptKind.userMessage);
      expect(TranscriptKind.fromWire('agent_thought'), TranscriptKind.agentThought);
      expect(TranscriptKind.fromWire('tool_result'), TranscriptKind.toolResult);
    });

    test('认不出的类型落到 unknown 而不是抛异常', () {
      expect(
        TranscriptKind.fromWire('some_future_kind'),
        TranscriptKind.unknown,
        reason: '一个新的事件类型不该让整段会话显示不出来',
      );
      expect(TranscriptKind.fromWire(null), TranscriptKind.unknown);
    });
  });

  group('ContentBlock', () {
    test('每种块都解析', () {
      expect(
        ContentBlock.fromJson({'type': 'text', 'text': 'hi'}),
        isA<TextBlock>(),
      );
      expect(
        ContentBlock.fromJson({'type': 'thought', 'text': 'hmm'}),
        isA<ThoughtBlock>(),
      );
      expect(
        ContentBlock.fromJson({
          'type': 'tool_call',
          'tool_call_id': 'c',
          'name': 'Bash',
          'input': {'command': 'ls'},
        }),
        isA<ToolCallBlock>(),
      );
      expect(
        ContentBlock.fromJson({
          'type': 'tool_result',
          'tool_call_id': 'c',
          'output': 'out',
          'is_error': true,
        }),
        isA<ToolResultBlock>(),
      );
      expect(
        ContentBlock.fromJson({
          'type': 'diff',
          'path': '/p',
          'old': 'a',
          'new': 'b',
        }),
        isA<DiffBlock>(),
      );
      expect(
        ContentBlock.fromJson({
          'type': 'plan',
          'entries': ['one'],
        }),
        isA<PlanBlock>(),
      );
      expect(
        ContentBlock.fromJson({'type': 'image', 'blob_ref': 'abc'}),
        isA<ImageBlock>(),
      );
      expect(
        ContentBlock.fromJson({
          'type': 'resource_ref',
          'generation_id': '01G',
          'offset': 0,
          'length': 10,
          'mime': 'text/plain',
        }),
        isA<ResourceRefBlock>(),
      );
    });

    test('认不出的块保留原名', () {
      final b = ContentBlock.fromJson({'type': 'some_future_block'});
      expect(b, isA<UnknownBlock>());
      expect(
        (b as UnknownBlock).rawType,
        'some_future_block',
        reason: '原名是唯一能告诉下一个人还有哪种块要处理的东西',
      );
    });

    test('diff 的 `new` 键映射到 newContent（Dart 关键字）', () {
      final b = ContentBlock.fromJson({
        'type': 'diff',
        'path': '/p',
        'old': 'before',
        'new': 'after',
      }) as DiffBlock;
      expect(b.oldContent, 'before');
      expect(
        b.newContent,
        'after',
        reason: '`new` 是 Dart 关键字，字段只能叫 newContent —— 但线上键仍然是 new',
      );
    });

    test('工具调用的一行摘要优先取命令', () {
      final b = ContentBlock.fromJson({
        'type': 'tool_call',
        'tool_call_id': 'c',
        'name': 'Bash',
        'input': {'command': 'cargo test'},
      }) as ToolCallBlock;
      expect(b.summary, 'cargo test');
    });

    test('没有可识别的参数时摘要退回工具名', () {
      final b = ContentBlock.fromJson({
        'type': 'tool_call',
        'tool_call_id': 'c',
        'name': 'Bash',
        'input': <String, dynamic>{},
      }) as ToolCallBlock;
      expect(b.summary, 'Bash');
    });
  });

  group('TranscriptEvent', () {
    test('解析一条事件，含出处', () {
      final e = TranscriptEvent.fromJson({
        'id': '01E',
        'seq': 3,
        'kind': 'tool_call',
        'occurred_at': '2026-10-29T08:00:00Z',
        'content': [
          {
            'type': 'tool_call',
            'tool_call_id': 'c',
            'name': 'Bash',
            'input': {'command': 'ls'},
          },
        ],
        'text': 'Bash: ls',
        'tool_name': 'Bash',
        'tool_call_id': 'c',
        'source': {'generation_id': '01G', 'offset': 1024, 'length': 512},
      });
      expect(e.seq, 3);
      expect(e.kind, TranscriptKind.toolCall);
      expect(e.toolName, 'Bash');
      expect(
        e.sourceGenerationId,
        '01G',
        reason: '出处是「凭什么这么认为」的最后一跳，界面要能指回原文',
      );
      expect(e.sourceOffset, 1024);
    });

    test('没有 text 时从内容块拼出纯文本', () {
      final e = TranscriptEvent.fromJson({
        'id': '01E',
        'seq': 0,
        'kind': 'agent_message',
        'content': [
          {'type': 'text', 'text': 'one'},
          {'type': 'text', 'text': 'two'},
        ],
      });
      expect(e.plainText, 'one\ntwo');
    });
  });
}
