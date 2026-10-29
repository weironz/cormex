import 'package:cormex_app/features/sessions/widgets/event_tile.dart';
import 'package:cormex_app/models/transcript_event.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

Widget _wrap(Widget child) => MaterialApp(home: Scaffold(body: child));

void main() {
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
    expect(find.text('用户'), findsOneWidget);
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
    // 「思考」出现两次：头部的类型标签，和折叠块的标题。点的是折叠块那个
    expect(find.text('思考'), findsNWidgets(2));
    expect(
      find.text('这是推理过程'),
      findsNothing,
      reason: '思考默认折叠：一段会话里思考块可能比对话还长',
    );

    await tester.tap(find.byType(ExpansionTile));
    await tester.pumpAndSettle();
    expect(find.text('这是推理过程'), findsOneWidget);
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
        reason: '$kind 是会话的一部分，但不是有人说过的话 —— 渲染出来会让一段对话里'
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
              ToolResultBlock(
                toolCallId: 'c',
                output: 'boom',
                isError: true,
              ),
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
      reason: '绝对日期，不用「3 分钟前」：会话可以被回放到任意时点，而相对时间的'
          '基准点是「现在」',
    );
  });
}
