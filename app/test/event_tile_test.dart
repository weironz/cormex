import 'package:cormex_app/features/sessions/widgets/event_tile.dart';
import 'package:cormex_app/models/transcript_event.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

Widget _wrap(Widget child) => MaterialApp(home: Scaffold(body: child));

void main() {
  testWidgets('旧档案的已知环境状态不会显示为 Codex 答复', (tester) async {
    await tester.pumpWidget(
      _wrap(
        const EventTile(
          agent: 'codex',
          event: TranscriptEvent(
            id: 'env',
            seq: 0,
            kind: TranscriptKind.agentMessage,
            content: [TextBlock('Not a git repo, skipping')],
          ),
        ),
      ),
    );
    expect(find.text('环境状态'), findsOneWidget);
    expect(find.text('Codex'), findsNothing);
    await tester.tap(find.text('环境状态'));
    await tester.pumpAndSettle();
    expect(find.text('Not a git repo, skipping'), findsOneWidget);
  });
  test('对话视图只隐藏可识别的内部消息，普通问答保留', () {
    TranscriptEvent message(TranscriptKind kind, String text, {String? role}) =>
        TranscriptEvent(
          id: 'test',
          seq: 0,
          kind: kind,
          role: role,
          content: [TextBlock(text)],
        );
    expect(
      isConversationEvent(
        message(TranscriptKind.userMessage, '帮我检查项目'),
        agent: 'codex',
      ),
      isTrue,
    );
    expect(
      isConversationEvent(
        message(TranscriptKind.agentMessage, '已经完成'),
        agent: 'codex',
      ),
      isTrue,
    );
    expect(
      isConversationEvent(
        message(
          TranscriptKind.userMessage,
          '<environment_context>secret</environment_context>',
        ),
        agent: 'codex',
      ),
      isFalse,
    );
    expect(
      isConversationEvent(
        message(
          TranscriptKind.userMessage,
          '<subagent_notification>{}</subagent_notification>',
        ),
        agent: 'codex',
      ),
      isFalse,
    );
    expect(
      isConversationEvent(
        message(TranscriptKind.agentMessage, 'Not a git repo, skipping'),
        agent: 'codex',
      ),
      isFalse,
    );
    expect(
      isConversationEvent(
        message(TranscriptKind.agentMessage, 'Not a git repo, skipping'),
        agent: 'claude_code',
      ),
      isTrue,
    );
    expect(
      isConversationEvent(
        message(TranscriptKind.agentMessage, '内部内容', role: 'developer'),
      ),
      isFalse,
    );
  });

  test('仅隐藏来源明确标记为过程的助手消息', () {
    TranscriptEvent answer(String? phase) => TranscriptEvent(
      id: 'a',
      seq: 1,
      kind: TranscriptKind.agentMessage,
      role: 'assistant',
      phase: phase,
      content: const [TextBlock('内容')],
    );
    expect(isConversationEvent(answer('commentary'), agent: 'codex'), isFalse);
    expect(isConversationEvent(answer('final_answer'), agent: 'codex'), isTrue);
    expect(isConversationEvent(answer(null), agent: 'codex'), isTrue);
    final direct = TranscriptEvent(
      id: 'b',
      seq: 2,
      kind: TranscriptKind.agentMessage,
      role: 'assistant',
      content: const [TextBlock('Not a git repo, skipping')],
    );
    expect(isConversationEvent(direct, agent: 'codex'), isTrue);
  });
  testWidgets('自动注入的开发者上下文默认折叠但可查看', (tester) async {
    await tester.pumpWidget(
      _wrap(
        const EventTile(
          event: TranscriptEvent(
            id: 'context',
            seq: 0,
            kind: TranscriptKind.agentMessage,
            role: 'developer',
            content: [TextBlock('内部上下文')],
          ),
        ),
      ),
    );
    expect(find.text('聊天上下文'), findsOneWidget);
    expect(find.text('内部上下文'), findsNothing);
    await tester.tap(find.byType(ExpansionTile));
    await tester.pumpAndSettle();
    expect(find.text('内部上下文'), findsOneWidget);
  });

  testWidgets('旧归档未保存角色时仍折叠可识别的 Codex 上下文', (tester) async {
    await tester.pumpWidget(
      _wrap(
        const EventTile(
          event: TranscriptEvent(
            id: 'old-context',
            seq: 0,
            kind: TranscriptKind.agentMessage,
            content: [
              TextBlock('You are Codex, an agent based on a test model.'),
            ],
          ),
        ),
      ),
    );
    expect(find.text('聊天上下文'), findsOneWidget);
    expect(
      find.text('You are Codex, an agent based on a test model.'),
      findsNothing,
    );
  });
  testWidgets('对话以可选择的 Markdown 展示并不自动加载远程图片', (tester) async {
    await tester.pumpWidget(
      _wrap(
        const EventTile(
          event: TranscriptEvent(
            id: 'md',
            seq: 0,
            kind: TranscriptKind.agentMessage,
            content: [
              TextBlock(
                '# 标题\n\n**重点**\n\n```rust\nlet x = 1;\n```\n\n![示例](https://example.invalid/image.png)',
              ),
            ],
          ),
        ),
      ),
    );
    final markdown = tester.widget<MarkdownBody>(find.byType(MarkdownBody));
    expect(markdown.selectable, isTrue);
    expect(find.text('[图片: 示例]'), findsOneWidget);
    expect(find.byType(Image), findsNothing);
  });
  testWidgets('用户消息显示正文', (tester) async {
    await tester.pumpWidget(
      _wrap(
        const EventTile(
          event: TranscriptEvent(
            id: '01E',
            seq: 0,
            kind: TranscriptKind.userMessage,
            content: [TextBlock('对象存储用的是 RustFS')],
          ),
        ),
      ),
    );
    expect(find.text('你'), findsOneWidget);
    expect(find.text('对象存储用的是 RustFS'), findsOneWidget);
  });

  testWidgets('思考默认折叠，点开才显示', (tester) async {
    await tester.pumpWidget(
      _wrap(
        const EventTile(
          event: TranscriptEvent(
            id: '01E',
            seq: 0,
            kind: TranscriptKind.agentThought,
            content: [ThoughtBlock('这是推理过程')],
          ),
        ),
      ),
    );
    expect(find.text('思考'), findsOneWidget);
    expect(find.text('这是推理过程'), findsNothing, reason: '思考默认折叠：一段聊天里思考块可能比对话还长');

    await tester.tap(find.byType(ExpansionTile));
    await tester.pumpAndSettle();
    expect(find.text('这是推理过程'), findsOneWidget);
  });

  testWidgets('子任务通知不冒充用户并可查看格式化结果及原文', (tester) async {
    const raw =
        '<subagent_notification>{"status":{"completed":"第一行\\n\\n**结果**"}}</subagent_notification>';
    await tester.pumpWidget(
      _wrap(
        const EventTile(
          event: TranscriptEvent(
            id: 'notice',
            seq: 0,
            kind: TranscriptKind.userMessage,
            content: [TextBlock(raw)],
          ),
        ),
      ),
    );
    expect(find.text('子任务通知'), findsOneWidget);
    expect(find.text('你'), findsNothing);
    await tester.tap(find.byType(ExpansionTile).first);
    await tester.pumpAndSettle();
    expect(find.byType(MarkdownBody), findsOneWidget);
    expect(find.text('查看原文'), findsOneWidget);
  });

  testWidgets('meta 与 unknown 不渲染', (tester) async {
    for (final kind in [TranscriptKind.meta, TranscriptKind.unknown]) {
      await tester.pumpWidget(
        _wrap(
          EventTile(
            event: TranscriptEvent(id: '01E', seq: 0, kind: kind),
          ),
        ),
      );
      expect(
        find.byType(Text),
        findsNothing,
        reason:
            '$kind 是聊天的一部分，但不是有人说过的话 —— 渲染出来会让一段对话里'
            '夹杂几十条「mode」「atis-latch」',
      );
    }
  });

  testWidgets('工具出错时标题是「工具出错」', (tester) async {
    await tester.pumpWidget(
      _wrap(
        const EventTile(
          event: TranscriptEvent(
            id: '01E',
            seq: 0,
            kind: TranscriptKind.toolResult,
            content: [
              ToolResultBlock(toolCallId: 'c', output: 'boom', isError: true),
            ],
          ),
        ),
      ),
    );
    expect(find.text('工具出错'), findsOneWidget);
  });

  testWidgets('时间用绝对日期', (tester) async {
    await tester.pumpWidget(
      _wrap(
        EventTile(
          event: TranscriptEvent(
            id: '01E',
            seq: 0,
            kind: TranscriptKind.userMessage,
            occurredAt: DateTime(2026, 10, 29, 8, 0),
            content: const [TextBlock('hi')],
          ),
        ),
      ),
    );
    expect(
      find.textContaining('2026年10月29日'),
      findsOneWidget,
      reason:
          '绝对日期，不用「3 分钟前」：聊天可以被回放到任意时点，而相对时间的'
          '基准点是「现在」',
    );
  });
}
