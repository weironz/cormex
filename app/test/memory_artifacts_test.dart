import 'dart:async';
import 'dart:convert';

import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/core/formatting.dart';
import 'package:cormex_app/features/memory/memory_browser.dart';
import 'package:cormex_app/features/settings/settings_page.dart';
import 'package:cormex_app/features/memory/widgets/artifact_sheet.dart';
import 'package:cormex_app/features/memory/widgets/fact_card.dart';
import 'package:cormex_app/features/settings/memory_trial_section.dart';
import 'package:cormex_app/models/memory_artifact.dart';
import 'package:cormex_app/models/memory_context.dart';
import 'package:cormex_app/models/memory_extraction_status.dart';
import 'package:cormex_app/models/memory_fact.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:cormex_app/state/memory_artifacts.dart';
import 'package:cormex_app/state/memory_extraction.dart';
import 'package:flutter/material.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

http.Response jsonResponse(Object body) => http.Response(
  jsonEncode(body),
  200,
  headers: {'content-type': 'application/json; charset=utf-8'},
);

Map<String, Object?> artifactJson(String id, {String kind = 'summary'}) => {
  'id': id,
  'kind': kind,
  'title': '记录 $id',
  'markdown': '## 问答摘要\n\n- 比较方案\n- 保留 `runc` 回滚\n',
  'project_name': 'cortex',
  'session_id': 'chat',
  'chat_title': '来源会话',
  'status': kind,
  'source_event_ids': ['question', 'answer'],
  'reason': '整理本轮讨论与结果，便于后续查看。',
  'evidence': [
    {'message_id': 'question', 'role': 'user', 'quote': '比较这两个方案。'},
    {'message_id': 'answer', 'role': 'assistant', 'quote': '可以先验证沙箱隔离。'},
  ],
  'created_at': '2026-11-01T08:00:00Z',
};

Map<String, Object?> extractionStatus() => {
  'enabled': false,
  'use_server_model': true,
  'provider': 'openai',
  'model': 'test-model',
  'model_configured': true,
  'phase': 'paused',
  'total_chats': 2300,
  'processed_chats': 0,
  'pending_chats': 2300,
  'active_facts': 0,
  'memory_count': 2,
  'suggestion_count': 1,
  'temporary_count': 1,
  'summary_count': 2,
  'legacy_count': 36,
};

CormexApi apiFor(Future<http.Response> Function(http.Request) handler) =>
    CormexApi(baseUrl: 'http://localhost', client: MockClient(handler));

Widget scoped(CormexApi api, Widget child) => ProviderScope(
  overrides: [apiProvider.overrideWithValue(api)],
  child: MaterialApp(home: child),
);

Future<void> disposePage(WidgetTester tester) async {
  await tester.pumpWidget(const SizedBox());
  await tester.pump();
}

Future<void> selectCategory(WidgetTester tester, String name) async {
  final chip = find.byKey(ValueKey('memory-category-$name'));
  await tester.ensureVisible(chip);
  await tester.pumpAndSettle();
  await tester.tap(chip);
  await tester.pumpAndSettle();
}

void main() {
  test(
    'artifact API encodes kind and cursor and preserves source evidence',
    () async {
      final api = apiFor((r) async {
        expect(r.method, 'GET');
        expect(r.url.path, '/memory/artifacts');
        expect(r.url.queryParameters, {
          'kind': 'summary',
          'limit': '30',
          'cursor': 'next+/= 中文',
        });
        return jsonResponse({
          'items': [artifactJson('one')],
          'next_cursor': 'after',
        });
      });
      addTearDown(api.dispose);
      final page = await api.memoryArtifacts(
        kind: MemoryArtifactKind.summary,
        cursor: 'next+/= 中文',
      );
      expect(page.nextCursor, 'after');
      expect(page.items.single.sourceEventIds, ['question', 'answer']);
      expect(page.items.single.evidence.last.role, 'assistant');
      expect(page.items.single.evidence.last.messageId, 'answer');
      expect(page.items.single.statusLabel, '会话摘要');
    },
  );

  test(
    'verification describes evidence strength without a manual approval step',
    () {
      expect(
        MemoryArtifact.fromJson(artifactJson('summary')).statusLabel,
        '会话摘要',
      );
      MemoryContext contextWith(
        String? verification, {
        String role = 'mixed',
      }) => MemoryContext.fromJson({
        'fact_id': 'f',
        'session_id': 's',
        'source_role': role,
        'verification': verification,
      });
      expect(contextWith('evidence_supported').verificationLabel, '归档工具佐证');
      expect(contextWith('user_stated').verificationLabel, '原对话给定');
      expect(contextWith('reported').verificationDescription, '来自助手自述，未经独立验证。');
      expect(
        contextWith(null, role: 'assistant').verificationDescription,
        '来自助手自述，未经独立验证。',
      );
      expect(contextWith('future_verification').verificationLabel, '依据原对话');
    },
  );

  test(
    'memory counters preserve zero and distinguish retained memories from writes',
    () {
      final status = MemoryExtractionStatus.fromJson({
        ...extractionStatus(),
        'memory_count': 0,
        'active_facts': 36,
        'facts_created': null,
        'trial': {
          'status': 'completed',
          'processed_chats': 2,
          'memory_count': 2,
          'confirmed_count': 99,
          'suggestion_count': 1,
          'temporary_count': 1,
          'summary_count': 2,
          'facts_created': 0,
        },
      });
      expect(status.memoryCount, 0);
      expect(status.outputCountLabels, ['可用记忆 0', '会话摘要 2']);
      expect(status.hasAnalyzedChats, isTrue);
      expect(status.trialMemoryCount, 2);
      expect(status.trialCompletedWithNoMemories, isFalse);
      expect(status.trialOutputCountLabels, ['自动保留记忆 2', '实际写入 0', '会话摘要 2']);
      final old = MemoryExtractionStatus.fromJson({
        'active_facts': 3,
        'facts_created': 36,
        'trial': {'confirmed_count': 9, 'facts_created': 0},
      });
      expect(old.memoryCount, 3);
      expect(old.trialMemoryCount, isNull);
      expect(old.trialOutputCountLabels, ['自动保留记忆 —', '实际写入 0', '会话摘要 —']);
      expect(
        MemoryExtractionStatus.fromJson({'facts_created': 36}).memoryCount,
        isNull,
      );
    },
  );

  test(
    'summaries load lazily, cache pages and retry the same cursor',
    () async {
      final requests = <Uri>[];
      var failed = false;
      final api = apiFor((r) async {
        requests.add(r.url);
        final kind = r.url.queryParameters['kind']!;
        expect(kind, 'summary');
        final cursor = r.url.queryParameters['cursor'];
        if (cursor != null && !failed) {
          failed = true;
          return http.Response('{"error":"temporary"}', 503);
        }
        return jsonResponse({
          'items': [
            artifactJson('$kind-first', kind: kind),
            if (cursor != null) artifactJson('$kind-second', kind: kind),
          ],
          if (cursor == null) 'next_cursor': 'more-$kind',
        });
      });
      final c = ProviderContainer(
        overrides: [apiProvider.overrideWithValue(api)],
      );
      addTearDown(c.dispose);
      addTearDown(api.dispose);
      final notifier = c.read(memoryArtifactsProvider.notifier);
      expect(requests, isEmpty);
      await notifier.load(MemoryArtifactKind.summary);
      await notifier.load(MemoryArtifactKind.summary);
      expect(requests.length, 1);
      expect(requests.single.queryParameters['limit'], '30');
      await notifier.load(MemoryArtifactKind.summary, more: true);
      var state = c.read(memoryArtifactsProvider)[MemoryArtifactKind.summary]!;
      expect(state.items.length, 1);
      expect(state.error, isNotNull);
      expect(state.nextCursor, 'more-summary');
      await notifier.load(MemoryArtifactKind.summary, more: true);
      state = c.read(memoryArtifactsProvider)[MemoryArtifactKind.summary]!;
      expect(state.items.map((i) => i.id), ['summary-first', 'summary-second']);
      expect(state.nextCursor, isNull);
      expect(
        requests[1].queryParameters['cursor'],
        requests[2].queryParameters['cursor'],
      );
      expect(
        c
            .read(memoryArtifactsProvider)[MemoryArtifactKind.summary]!
            .items
            .length,
        2,
      );
      expect(requests.length, 3);
    },
  );

  test(
    'invalidating analysis ignores in-flight pages and loads only the active category',
    () async {
      final first = Completer<http.Response>();
      final started = Completer<void>();
      var calls = 0;
      final api = apiFor((r) async {
        expect(r.url.queryParameters['kind'], 'summary');
        calls++;
        if (calls == 1) {
          started.complete();
          return first.future;
        }
        return jsonResponse({
          'items': [artifactJson('new')],
        });
      });
      final c = ProviderContainer(
        overrides: [apiProvider.overrideWithValue(api)],
      );
      addTearDown(c.dispose);
      addTearDown(api.dispose);
      final notifier = c.read(memoryArtifactsProvider.notifier);
      notifier.select(MemoryArtifactKind.summary);
      await started.future;
      notifier.invalidate();
      await Future<void>.delayed(Duration.zero);
      first.complete(
        jsonResponse({
          'items': [artifactJson('old')],
        }),
      );
      await Future<void>.delayed(Duration.zero);
      expect(calls, 2);
      expect(
        c
            .read(memoryArtifactsProvider)[MemoryArtifactKind.summary]!
            .items
            .single
            .id,
        'new',
      );
    },
  );

  testWidgets(
    'only memory and summaries are visible and summaries are requested on demand',
    (tester) async {
      tester.view.physicalSize = const Size(1440, 1100);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.resetPhysicalSize);
      addTearDown(tester.view.resetDevicePixelRatio);
      final kinds = <String>[];
      final api = apiFor((r) async {
        expect(r.method, 'GET');
        if (r.url.path == '/memory/search') return jsonResponse({'facts': []});
        if (r.url.path == '/memory/artifacts') {
          final kind = r.url.queryParameters['kind']!;
          kinds.add(kind);
          return jsonResponse({
            'items': [artifactJson(kind, kind: kind)],
          });
        }
        return jsonResponse(extractionStatus());
      });
      await tester.pumpWidget(scoped(api, const MemoryBrowser()));
      await tester.pumpAndSettle();
      expect(kinds, isEmpty);
      expect(find.byType(ChoiceChip), findsNWidgets(2));
      expect(find.text('记忆'), findsOneWidget);
      expect(find.text('会话摘要'), findsOneWidget);
      for (final oldLabel in ['待确认', '临时状态', '旧策略', '已确认长期']) {
        expect(find.textContaining(oldLabel), findsNothing);
      }
      await selectCategory(tester, 'summary');
      expect(kinds, ['summary']);
      expect(find.text('阅读摘要'), findsOneWidget);
      await selectCategory(tester, 'memory');
      await selectCategory(tester, 'summary');
      expect(kinds, ['summary']);
      expect(tester.takeException(), isNull);
      await disposePage(tester);
    },
  );

  testWidgets(
    'zero retained memories is neutral and full extraction stays off',
    (tester) async {
      final api = apiFor((r) async {
        if (r.url.path == '/memory/search') return jsonResponse({'facts': []});
        expect(r.url.path, isNot('/memory/artifacts'));
        return jsonResponse({
          ...extractionStatus(),
          'memory_count': 0,
          'trial': {
            'status': 'completed',
            'selections': [{}, {}],
            'processed_chats': 2,
            'processed_messages': 4,
            'total_messages': 4,
            'memory_count': 0,
            'facts_created': 0,
            'confirmed_count': 0,
            'suggestion_count': 1,
            'temporary_count': 1,
            'summary_count': 2,
          },
        });
      });
      // The memory page no longer carries the extraction panel at all
      await tester.pumpWidget(scoped(api, const MemoryBrowser()));
      await tester.pumpAndSettle();
      expect(find.textContaining('本次保留 0 条记忆'), findsNothing);
      expect(tester.takeException(), isNull);
      await disposePage(tester);

      // The extraction panel, on the settings page
      await tester.pumpWidget(scoped(api, const SettingsPage()));
      await tester.pumpAndSettle();
      expect(find.textContaining('本次保留 0 条记忆'), findsOneWidget);
      expect(find.text('自动保留记忆 0'), findsOneWidget);
      expect(find.text('可用记忆 0'), findsOneWidget);
      expect(find.textContaining('验证通过'), findsNothing);
      expect(find.textContaining('待确认'), findsNothing);
      expect(find.text('提取失败'), findsNothing);
      expect(tester.widget<Switch>(find.byType(Switch)).value, isFalse);
      expect(find.textContaining('旧策略'), findsNothing);
      // Once in the full-extraction counts, once in the trial's
      expect(find.text('会话摘要 2'), findsNWidgets(2));
      expect(tester.takeException(), isNull);
      await disposePage(tester);
    },
  );

  for (final phone in [false, true]) {
    testWidgets(
      'summary detail renders Markdown and opens source chat (${phone ? 'phone' : 'desktop'})',
      (tester) async {
        tester.view.physicalSize = phone
            ? const Size(390, 844)
            : const Size(1440, 1000);
        tester.view.devicePixelRatio = 1;
        addTearDown(tester.view.resetPhysicalSize);
        addTearDown(tester.view.resetDevicePixelRatio);
        final artifact = MemoryArtifact.fromJson(artifactJson('summary'));
        final api = apiFor((r) async {
          if (r.url.path.endsWith('/events')) {
            return jsonResponse({'events': []});
          }
          return jsonResponse({
            'session': {'id': 'chat', 'agent': 'codex', 'title': '来源会话'},
          });
        });
        await tester.pumpWidget(
          scoped(
            api,
            Scaffold(
              body: Builder(
                builder: (context) => TextButton(
                  onPressed: () => showArtifactSheet(context, artifact),
                  child: const Text('打开摘要'),
                ),
              ),
            ),
          ),
        );
        await tester.tap(find.text('打开摘要'));
        await tester.pumpAndSettle();
        final dialog = tester.getRect(find.byType(Dialog));
        if (phone) {
          expect(dialog.height, 844);
        } else {
          expect(dialog.width, greaterThanOrEqualTo(1100));
          expect(dialog.center.dy, closeTo(500, 1));
        }
        expect(find.text('会话摘要详情'), findsOneWidget);
        expect(
          find.byWidgetPredicate(
            (w) => w is MarkdownBody && w.data == artifact.markdown,
          ),
          findsOneWidget,
        );
        expect(find.text('比较这两个方案。'), findsOneWidget);
        await tester.ensureVisible(find.text('查看来源聊天'));
        await tester.pumpAndSettle();
        await tester.tap(find.text('查看来源聊天'));
        await tester.pumpAndSettle();
        expect(find.text('来源会话'), findsOneWidget);
        expect(find.text('会话摘要详情'), findsNothing);
        expect(tester.takeException(), isNull);
        await disposePage(tester);
      },
    );
  }

  testWidgets(
    'qa_turn and mixed role describe full Q&A while retaining source reading',
    (tester) async {
      const text = '## 用户提问\n\n请使用中文。\n\n## AI 答复\n\n好的。';
      final api = apiFor((r) async {
        if (r.url.path == '/memory/context') {
          return jsonResponse({
            'contexts': [
              {
                'fact_id': 'fact',
                'subject': '交互偏好',
                'session_id': 'chat',
                'source_role': 'mixed',
                'extraction_unit': 'qa_turn',
                'verification': 'evidence_supported',
                'reason': '工具测试记录支持该结果，可供后续任务复用。',
              },
            ],
          });
        }
        if (r.url.path.startsWith('/episodes/')) {
          return jsonResponse({
            'id': 'qa',
            'session_id': 'chat',
            'role': 'mixed',
            'text': text,
          });
        }
        return jsonResponse({
          'session': {'id': 'chat', 'agent': 'codex', 'title': '完整问答来源'},
        });
      });
      await tester.pumpWidget(
        scoped(
          api,
          const Scaffold(
            body: FactCard(
              fact: MemoryFact(
                id: 'fact',
                statement: '偏好中文',
                sourceEpisodeId: 'qa',
              ),
            ),
          ),
        ),
      );
      await tester.tap(find.text('查看详情'));
      await tester.pumpAndSettle();
      expect(find.textContaining('提取粒度：完整问答'), findsOneWidget);
      expect(find.text('自动保留依据'), findsOneWidget);
      expect(find.text('证据：归档工具佐证'), findsOneWidget);
      expect(find.text('工具测试记录支持该结果，可供后续任务复用。'), findsOneWidget);
      expect(find.textContaining('未提供逐条保留原因'), findsNothing);
      expect(find.textContaining('旧提取没有同时携带配对问答'), findsNothing);
      expect(
        find.byWidgetPredicate((w) => w is MarkdownBody && w.data == text),
        findsOneWidget,
      );
      expect(find.text('查看来源聊天'), findsOneWidget);
      expect(tester.takeException(), isNull);
      await disposePage(tester);
    },
  );

  for (final evidence in [
    (
      verification: 'evidence_supported',
      label: '归档工具佐证',
      description: '原对话包含归档的工具或测试记录。',
    ),
    (verification: 'reported', label: '助手自述', description: '来自助手自述，未经独立验证。'),
    (verification: 'user_stated', label: '原对话给定', description: '来自原对话给定的信息。'),
  ]) {
    testWidgets(
      'memory card and detail explain ${evidence.verification} without review controls',
      (tester) async {
        const topicMarkdown =
            '## 主题\n\n沙箱运行策略\n\n## 结论\n\n保留 `runc` 回滚。\n\n## 适用场景\n\n后续沙箱部署与故障恢复。';
        final contextInfo = MemoryContext.fromJson({
          'fact_id': 'f',
          'session_id': 'chat',
          'subject': '项目背景',
          'source_role': 'assistant',
          'extraction_unit': 'qa_turn',
          'verification': evidence.verification,
          'reason': '基于对话中可复用的信息自动保留。',
        });
        final api = apiFor((r) async {
          if (r.url.path.startsWith('/episodes/')) {
            return jsonResponse({
              'id': 'ep',
              'session_id': 'chat',
              'role': 'assistant',
              'text': '## 来源内容\n\n保留原始对话。',
            });
          }
          return jsonResponse({
            'session': {'id': 'chat', 'agent': 'codex', 'title': '来源会话'},
          });
        });
        await tester.pumpWidget(
          scoped(
            api,
            Scaffold(
              body: Center(
                child: SizedBox(
                  width: 400,
                  child: FactCard(
                    fact: const MemoryFact(
                      id: 'f',
                      statement: topicMarkdown,
                      predicate: 'memory/sandbox-runtime',
                      sourceEpisodeId: 'ep',
                    ),
                    contextInfo: contextInfo,
                  ),
                ),
              ),
            ),
          ),
        );
        expect(find.text('完整问答 · ${evidence.label}'), findsOneWidget);
        expect(find.text('项目记忆'), findsOneWidget);
        expect(find.textContaining('memory/sandbox-runtime'), findsNothing);
        expect(
          find.byWidgetPredicate(
            (w) => w is MarkdownBody && w.data == plainStatement(topicMarkdown),
          ),
          findsOneWidget,
        );
        await tester.tap(find.text('查看详情'));
        await tester.pumpAndSettle();
        expect(find.text('证据：${evidence.label}'), findsOneWidget);
        expect(find.text(evidence.description), findsOneWidget);
        expect(find.text('自动保留依据'), findsOneWidget);
        expect(find.text('类型：项目记忆'), findsOneWidget);
        expect(
          find.descendant(
            of: find.byType(Dialog),
            matching: find.byWidgetPredicate(
              (w) => w is MarkdownBody && w.data == plainStatement(topicMarkdown),
            ),
          ),
          findsOneWidget,
        );
        for (final oldCopy in ['待确认', '待核对', '用户已确认', '需要审核']) {
          expect(find.textContaining(oldCopy), findsNothing);
        }
        await tester.ensureVisible(find.text('提取属性'));
        await tester.pumpAndSettle();
        await tester.tap(find.text('提取属性'));
        await tester.pumpAndSettle();
        expect(find.textContaining('人工核实'), findsNothing);
        expect(find.textContaining('自评分数用于说明'), findsOneWidget);
        expect(find.textContaining('关系类型：项目记忆'), findsOneWidget);
        expect(find.textContaining('memory/sandbox-runtime'), findsNothing);
        expect(find.text('查看来源聊天'), findsOneWidget);
        expect(tester.takeException(), isNull);
        await disposePage(tester);
      },
    );
  }

  testWidgets(
    'trial section shows retained memories and actual writes even when totals are absent',
    (tester) async {
      final api = apiFor((r) async => jsonResponse({'embedding_ready': true}));
      final status = MemoryExtractionStatus.fromJson({
        ...extractionStatus(),
        'trial': {
          'status': 'completed',
          'selections': [],
          'processed_chats': 2,
          'memory_count': 2,
          'confirmed_count': 0,
          'suggestion_count': 0,
          'temporary_count': 0,
          'summary_count': 2,
          'facts_created': 0,
          'processed_messages': null,
          'total_messages': 0,
        },
      });
      await tester.pumpWidget(
        scoped(
          api,
          Scaffold(
            body: SingleChildScrollView(
              child: Padding(
                padding: const EdgeInsets.all(24),
                child: MemoryTrialSection(status: status),
              ),
            ),
          ),
        ),
      );
      await tester.pumpAndSettle();
      expect(find.text('自动保留记忆 2'), findsOneWidget);
      expect(find.text('实际写入 0'), findsOneWidget);
      expect(find.textContaining('已确认'), findsNothing);
      expect(find.textContaining('待确认'), findsNothing);
      expect(find.text('会话摘要 2'), findsOneWidget);
      expect(
        tester
            .widget<LinearProgressIndicator>(
              find.byType(LinearProgressIndicator),
            )
            .value,
        1,
      );
      expect(tester.takeException(), isNull);
      await disposePage(tester);
    },
  );

  testWidgets(
    'new analysis refreshes only the active category and clears inactive caches',
    (tester) async {
      final kinds = <String>[];
      var completed = false;
      final api = apiFor((r) async {
        if (r.url.path == '/memory/search') return jsonResponse({'facts': []});
        if (r.url.path == '/memory/artifacts') {
          final kind = r.url.queryParameters['kind']!;
          kinds.add(kind);
          return jsonResponse({
            'items': [
              artifactJson('$kind-${completed ? 'new' : 'old'}', kind: kind),
            ],
          });
        }
        return jsonResponse({
          ...extractionStatus(),
          'summary_count': completed ? 3 : 2,
        });
      });
      await tester.pumpWidget(scoped(api, const MemoryBrowser()));
      await tester.pumpAndSettle();
      await selectCategory(tester, 'summary');
      completed = true;
      final container = ProviderScope.containerOf(
        tester.element(find.byType(MemoryBrowser)),
      );
      container.invalidate(memoryExtractionProvider);
      await tester.pumpAndSettle();
      expect(kinds, ['summary', 'summary']);
      expect(find.text('记录 summary-new'), findsOneWidget);
      await selectCategory(tester, 'memory');
      completed = false;
      container.invalidate(memoryExtractionProvider);
      await tester.pumpAndSettle();
      expect(kinds, ['summary', 'summary']);
      await selectCategory(tester, 'summary');
      expect(kinds, ['summary', 'summary', 'summary']);
      expect(find.text('记录 summary-old'), findsOneWidget);
      expect(tester.takeException(), isNull);
      await disposePage(tester);
    },
  );
}
