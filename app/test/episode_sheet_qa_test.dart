import 'dart:convert';

import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/features/memory/widgets/episode_sheet.dart';
import 'package:cormex_app/models/memory_context.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

// Matches the backend's pretty-printed TurnInput, including escaped newlines
// and metadata that must survive the raw-view round trip unchanged.
const qaJson = r'''{
  "session_id": "chat",
  "chat_title": "QA 完整问答",
  "project_name": "cormex",
  "source_mode": "conversation",
  "messages": [
    {
      "id": "question",
      "seq": 10,
      "role": "user",
      "text": "## 用户原文\n\n第一行\n第二行\n\n- 中文输出",
      "occurred_at": "2026-11-01T08:00:00Z"
    },
    {
      "id": "answer",
      "seq": 11,
      "role": "assistant",
      "text": "## AI 原文\n\n已收到。\n保留 `runc` 回滚。",
      "occurred_at": "2026-11-01T08:00:01Z"
    }
  ],
  "recent_context": [
    {
      "id": "earlier-question",
      "seq": 8,
      "role": "user",
      "text": "较早的问题\n\n只作为有限背景。",
      "occurred_at": "2026-11-01T07:00:00Z"
    },
    {
      "id": "earlier-answer",
      "seq": 9,
      "role": "assistant",
      "text": "较早的答复\n\n尚未确认。",
      "occurred_at": "2026-11-01T07:00:01Z"
    }
  ],
  "previous_summary": "## 之前的摘要\n\n等待用户确认。"
}''';

const questionMarkdown = '## 用户原文\n\n第一行\n第二行\n\n- 中文输出';
const answerMarkdown = '## AI 原文\n\n已收到。\n保留 `runc` 回滚。';

Future<void> openSource(
  WidgetTester tester, {
  String text = qaJson,
  String unit = 'qa_turn',
}) async {
  final api = CormexApi(
    baseUrl: 'http://localhost',
    client: MockClient(
      (request) async => http.Response(
        jsonEncode(
          request.url.path.startsWith('/episodes/')
              ? {
                  'id': 'qa',
                  'session_id': 'chat',
                  'role': 'system',
                  'text': text,
                }
              : {
                  'session': {
                    'id': 'chat',
                    'agent': 'codex',
                    'title': 'QA 完整问答',
                  },
                },
        ),
        200,
        headers: {'content-type': 'application/json; charset=utf-8'},
      ),
    ),
  );
  addTearDown(api.dispose);
  await tester.pumpWidget(
    ProviderScope(
      overrides: [apiProvider.overrideWithValue(api)],
      child: MaterialApp(
        home: Scaffold(
          body: Builder(
            builder: (context) => TextButton(
              onPressed: () => showEpisodeSheet(
                context,
                'qa',
                contextInfo: MemoryContext(
                  factId: 'fact',
                  subject: '偏好',
                  sessionId: 'chat',
                  sourceRole: 'mixed',
                  extractionUnit: unit,
                ),
              ),
              child: const Text('查看来源'),
            ),
          ),
        ),
      ),
    ),
  );
  await tester.tap(find.text('查看来源'));
  await tester.pumpAndSettle();
}

Future<void> tapVisible(WidgetTester tester, String text) async {
  await tester.ensureVisible(find.text(text));
  await tester.pumpAndSettle();
  await tester.tap(find.text(text));
  await tester.pumpAndSettle();
}

Future<void> disposeSource(WidgetTester tester) async {
  await tester.pumpWidget(const SizedBox());
  await tester.pump();
}

void main() {
  for (final phone in [false, true]) {
    testWidgets(
      'pretty QA JSON reads as role-based Markdown (${phone ? 'phone' : 'desktop'})',
      (tester) async {
        tester.view.physicalSize = phone
            ? const Size(390, 844)
            : const Size(1440, 1000);
        tester.view.devicePixelRatio = 1;
        addTearDown(tester.view.resetPhysicalSize);
        addTearDown(tester.view.resetDevicePixelRatio);
        await openSource(tester);

        expect(find.text('本轮完整问答'), findsOneWidget);
        expect(find.text('用户角色输入'), findsOneWidget);
        expect(find.text('AI 答复'), findsOneWidget);
        expect(find.text('system'), findsNothing);
        expect(
          tester
              .widgetList<MarkdownBody>(find.byType(MarkdownBody))
              .map((m) => m.data),
          [questionMarkdown, answerMarkdown],
        );
        expect(find.textContaining('session_id'), findsNothing);
        expect(find.textContaining('occurred_at'), findsNothing);
        expect(find.textContaining('source_mode'), findsNothing);
        expect(
          find.byWidgetPredicate(
            (w) =>
                w is SelectableText &&
                (w.data ?? w.textSpan?.toPlainText())?.contains('第一行\n第二行') ==
                    true,
          ),
          findsOneWidget,
        );
        expect(find.textContaining('较早的问题'), findsNothing);
        expect(find.text('此前会话摘要'), findsNothing);

        await tapVisible(tester, '有限背景');
        expect(find.text('此前会话摘要'), findsOneWidget);
        final background = tester
            .widgetList<MarkdownBody>(find.byType(MarkdownBody))
            .map((m) => m.data);
        expect(background, contains('较早的问题\n\n只作为有限背景。'));
        expect(background, contains('较早的答复\n\n尚未确认。'));
        expect(background, contains('## 之前的摘要\n\n等待用户确认。'));

        await tapVisible(tester, '原文');
        expect(find.byType(MarkdownBody), findsNothing);
        expect(
          find.byWidgetPredicate(
            (w) => w is SelectableText && w.data == qaJson,
          ),
          findsOneWidget,
        );
        expect(
          find.textContaining('"source_mode": "conversation"'),
          findsOneWidget,
        );
        expect(find.textContaining('"seq": 10'), findsOneWidget);
        expect(find.textContaining(r'第一行\n第二行'), findsOneWidget);

        await tapVisible(tester, '阅读视图');
        expect(
          tester
              .widgetList<MarkdownBody>(find.byType(MarkdownBody))
              .map((m) => m.data),
          [questionMarkdown, answerMarkdown],
        );
        expect(find.text('查看来源聊天'), findsOneWidget);
        expect(tester.takeException(), isNull);
        await disposeSource(tester);
      },
    );
  }

  for (final sample in [
    (unit: 'single_message', text: qaJson),
    (unit: 'qa_turn', text: '## 普通 Markdown\n\n未保存 JSON 的旧来源。'),
    (unit: 'qa_turn', text: '{"messages": [{"role": "user"}]}'),
  ]) {
    testWidgets(
      'preserves legacy or unparseable source (${sample.unit}, ${sample.text.length})',
      (tester) async {
        await openSource(tester, unit: sample.unit, text: sample.text);
        expect(
          find.byWidgetPredicate(
            (w) => w is MarkdownBody && w.data == sample.text,
          ),
          findsOneWidget,
        );
        expect(find.text('本轮完整问答'), findsNothing);
        expect(find.text('有限背景'), findsNothing);
        await tapVisible(tester, '原文');
        expect(
          find.byWidgetPredicate(
            (w) => w is SelectableText && w.data == sample.text,
          ),
          findsOneWidget,
        );
        expect(tester.takeException(), isNull);
        await disposeSource(tester);
      },
    );
  }
}
