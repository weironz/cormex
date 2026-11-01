import 'dart:convert';
import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/features/memory/memory_browser.dart';
import 'package:cormex_app/features/settings/memory_extraction_settings_page.dart';
import 'package:cormex_app/features/memory/widgets/fact_card.dart';
import 'package:cormex_app/models/memory_fact.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

Map<String, dynamic> status({
  bool enabled = false,
  String phase = 'paused',
  int done = 0,
}) => {
  'enabled': enabled,
  'phase': phase,
  'use_server_model': true,
  'provider': 'openai',
  'model': 'test-model',
  'model_configured': true,
  'providers': ['openai', 'ollama'],
  'total_chats': 2300,
  'processed_chats': done,
  'pending_chats': 2300 - done,
  'facts_created': 0,
};
http.Response json(Object value) => http.Response(
  jsonEncode(value),
  200,
  headers: {'content-type': 'application/json; charset=utf-8'},
);
Widget app(CormexApi api, {bool settings = false}) => ProviderScope(
  overrides: [apiProvider.overrideWithValue(api)],
  child: MaterialApp(
    home: settings
        ? const MemoryExtractionSettingsPage()
        : const MemoryBrowser(),
  ),
);
Future<void> dispose(WidgetTester tester) async {
  await tester.pumpWidget(const SizedBox());
  await tester.pump();
}

void main() {
  testWidgets(
    'desktop memory detail is centered, wide and renders source Markdown',
    (tester) async {
      tester.view.physicalSize = const Size(1440, 1000);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.resetPhysicalSize);
      addTearDown(tester.view.resetDevicePixelRatio);
      const source = '## 背景\n\n- 任务 A\n- 任务 B\n\n使用 `runsc`。';
      final api = CormexApi(
        baseUrl: 'http://localhost',
        client: MockClient((r) async {
          if (r.url.path == '/episodes/ep') {
            return json({
              'id': 'ep',
              'session_id': 'chat',
              'role': 'assistant',
              'text': source,
            });
          }
          if (r.url.path == '/memory/context') {
            return json({
              'contexts': [
                {
                  'fact_id': 'f',
                  'subject': 'Cortex 沙箱',
                  'session_id': 'chat',
                  'source_role': 'assistant',
                  'agent': 'codex',
                  'project_name': 'cortex',
                  'extraction_unit': 'single_message',
                },
              ],
            });
          }
          return json({
            'session': {'id': 'chat', 'agent': 'codex', 'title': '方案评审'},
          });
        }),
      );
      await tester.pumpWidget(
        ProviderScope(
          overrides: [apiProvider.overrideWithValue(api)],
          child: const MaterialApp(
            home: Scaffold(
              body: Center(
                child: SizedBox(
                  width: 400,
                  child: FactCard(
                    fact: MemoryFact(
                      id: 'f',
                      statement: '保留 `runc` 回滚。',
                      sourceEpisodeId: 'ep',
                    ),
                  ),
                ),
              ),
            ),
          ),
        ),
      );
      await tester.tap(find.text('查看详情'));
      await tester.pumpAndSettle();
      final rect = tester.getRect(find.byType(Dialog));
      expect(rect.width, greaterThanOrEqualTo(1100));
      expect(rect.center.dy, closeTo(500, 1));
      expect(find.text('Cortex 沙箱'), findsOneWidget);
      expect(find.textContaining('旧提取没有同时携带配对问答'), findsOneWidget);
      expect(
        find.byWidgetPredicate((w) => w is MarkdownBody && w.data == source),
        findsOneWidget,
      );
      expect(tester.takeException(), isNull);
      await dispose(tester);
    },
  );
  testWidgets('phone memory details use the full screen without overflow', (
    tester,
  ) async {
    tester.view.physicalSize = const Size(390, 844);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    final api = CormexApi(
      baseUrl: 'http://localhost',
      client: MockClient((r) async {
        if (r.url.path == '/episodes/ep') {
          return json({
            'id': 'ep',
            'session_id': 'chat',
            'role': 'user',
            'text': '## 原文\n\n用户输入。',
          });
        }
        return json({
          'contexts': [],
          'session': {'id': 'chat', 'agent': 'codex', 'title': '来源聊天'},
        });
      }),
    );
    const page = MaterialApp(
      home: Scaffold(
        body: FactCard(
          fact: MemoryFact(
            id: 'f',
            statement: '偏好使用中文。',
            sourceEpisodeId: 'ep',
          ),
        ),
      ),
    );
    await tester.pumpWidget(
      ProviderScope(
        overrides: [apiProvider.overrideWithValue(api)],
        child: page,
      ),
    );
    await tester.tap(find.text('查看详情'));
    await tester.pumpAndSettle();
    expect(tester.getRect(find.byType(Dialog)).height, 844);
    expect(tester.takeException(), isNull);
    await dispose(tester);
  });
  testWidgets('model configuration is reached through settings', (
    tester,
  ) async {
    final api = CormexApi(
      baseUrl: 'http://localhost',
      client: MockClient(
        (r) async => r.url.path == '/memory/search'
            ? json({'facts': []})
            : json(status()),
      ),
    );
    await tester.pumpWidget(app(api));
    await tester.pumpAndSettle();
    await tester.tap(find.text('前往设置'));
    await tester.pumpAndSettle();
    expect(find.text('设置'), findsWidgets);
    await tester.tap(find.text('模型配置'));
    await tester.pumpAndSettle();
    expect(find.text('模型与记忆提取'), findsOneWidget);
    expect(find.text('全量自动提取'), findsOneWidget);
    await dispose(tester);
  });
  testWidgets(
    'completed bounded job is distinct from the full extraction switch',
    (tester) async {
      final api = CormexApi(
        baseUrl: 'http://localhost',
        client: MockClient(
          (r) async => r.url.path == '/memory/search'
              ? json({'facts': []})
              : json({
                  ...status(),
                  'trial': {
                    'status': 'completed',
                    'selections': [{}, {}],
                    'processed_chats': 2,
                    'processed_messages': 4,
                    'total_messages': 4,
                    'facts_created': 3,
                  },
                }),
        ),
      );
      await tester.pumpWidget(app(api));
      await tester.pumpAndSettle();
      expect(find.textContaining('小样本分析完成'), findsOneWidget);
      expect(tester.widget<Switch>(find.byType(Switch)).value, isFalse);
      await dispose(tester);
    },
  );
  testWidgets('a memory opens its exact source text and the original chat', (
    tester,
  ) async {
    final api = CormexApi(
      baseUrl: 'http://localhost',
      client: MockClient((r) async {
        if (r.url.path == '/episodes/ep') {
          return json({
            'id': 'ep',
            'session_id': 'chat',
            'role': 'user',
            'text': '这段聊天的原始提问',
          });
        }
        if (r.url.path.endsWith('/events')) {
          return json({
            'events': [
              {
                'id': 'event',
                'seq': 0,
                'kind': 'user_message',
                'text': '这段聊天的原始提问',
              },
            ],
          });
        }
        return json({
          'session': {'id': 'chat', 'agent': 'codex', 'title': '真实源聊天'},
        });
      }),
    );
    await tester.pumpWidget(
      ProviderScope(
        overrides: [apiProvider.overrideWithValue(api)],
        child: const MaterialApp(
          home: Scaffold(
            body: Center(
              child: SizedBox(
                width: 400,
                child: FactCard(
                  fact: MemoryFact(
                    id: 'fact',
                    statement: 'Cortex 项目采用 Rust',
                    sourceEpisodeId: 'ep',
                  ),
                ),
              ),
            ),
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.text('查看详情'));
    await tester.pumpAndSettle();
    expect(find.text('记忆详情'), findsOneWidget);
    await tester.ensureVisible(find.text('原文'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('原文'));
    await tester.pumpAndSettle();
    expect(find.text('这段聊天的原始提问'), findsOneWidget);
    await tester.tap(find.text('查看来源聊天'));
    await tester.pumpAndSettle();
    expect(find.text('真实源聊天'), findsOneWidget);
    expect(tester.takeException(), isNull);
    await dispose(tester);
  });
  testWidgets(
    'unreachable status is not presented as extraction being disabled',
    (tester) async {
      final api = CormexApi(
        baseUrl: 'http://localhost',
        client: MockClient(
          (r) async => r.url.path == '/memory/search'
              ? json({'facts': []})
              : http.Response('unavailable', 503),
        ),
      );
      await tester.pumpWidget(app(api));
      await tester.pumpAndSettle();
      expect(find.text('无法读取记忆提取状态，请检查连接后重试。'), findsOneWidget);
      expect(find.text('全量未开启'), findsNothing);
      expect(tester.takeException(), isNull);
      await dispose(tester);
    },
  );
  testWidgets('hidden memory tab does not poll extraction status', (
    tester,
  ) async {
    var calls = 0;
    final api = CormexApi(
      baseUrl: 'http://localhost',
      client: MockClient((r) async {
        if (r.url.path == '/memory/search') return json({'facts': []});
        if (r.url.path == '/settings/memory-extraction') calls++;
        return json(status(enabled: true, phase: 'queued'));
      }),
    );
    await tester.pumpWidget(
      ProviderScope(
        overrides: [apiProvider.overrideWithValue(api)],
        child: const MaterialApp(
          home: TickerMode(enabled: false, child: MemoryBrowser()),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(calls, 1);
    await tester.pump(const Duration(seconds: 11));
    await tester.pumpAndSettle();
    expect(calls, 1);
    await dispose(tester);
  });
  testWidgets(
    'empty memory explains paused extraction and shows real archive totals',
    (tester) async {
      final api = CormexApi(
        baseUrl: 'http://localhost',
        client: MockClient(
          (r) async => r.url.path == '/memory/search'
              ? json({'facts': []})
              : json(status()),
        ),
      );
      await tester.pumpWidget(app(api));
      await tester.pumpAndSettle();
      expect(find.text('全量未开启'), findsOneWidget);
      expect(find.text('已检查 0 / 2300 个聊天'), findsOneWidget);
      expect(find.textContaining('但记忆提取尚未开启'), findsOneWidget);
      expect(find.text('前往设置'), findsOneWidget);
      await dispose(tester);
    },
  );
  testWidgets(
    'switch persists state and a failure shows retry without fake completion',
    (tester) async {
      var saved = false;
      final api = CormexApi(
        baseUrl: 'http://localhost',
        client: MockClient((r) async {
          if (r.url.path == '/memory/search') return json({'facts': []});
          if (r.method == 'PUT') {
            saved = (jsonDecode(r.body) as Map)['enabled'] == true;
          }
          return json({
            ...status(
              enabled: saved,
              phase: saved ? 'error' : 'paused',
              done: 12,
            ),
            if (saved) 'last_error': '模型提取失败，处理位置已保留',
          });
        }),
      );
      await tester.pumpWidget(app(api));
      await tester.pumpAndSettle();
      await tester.tap(find.byType(Switch));
      await tester.pumpAndSettle();
      expect(saved, isTrue);
      expect(find.text('提取失败'), findsOneWidget);
      expect(find.text('已检查 12 / 2300 个聊天'), findsOneWidget);
      expect(find.text('立即重试'), findsOneWidget);
      await dispose(tester);
    },
  );
  testWidgets(
    'custom model settings save on server and connection test sends no chat body',
    (tester) async {
      Map<String, dynamic>? saved;
      var tested = false;
      var current = status();
      final api = CormexApi(
        baseUrl: 'http://localhost',
        client: MockClient((r) async {
          if (r.method == 'PUT') {
            saved = (jsonDecode(r.body) as Map).cast<String, dynamic>();
            current = {
              ...current,
              ...saved!,
              'key_configured': true,
              'key_tail': '-key',
            };
          }
          if (r.url.path.endsWith('/test')) {
            tested = true;
            expect(r.body, '{}');
            return json({'ok': true});
          }
          return json(current);
        }),
      );
      await tester.pumpWidget(app(api, settings: true));
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(SwitchListTile, '使用服务端默认模型'));
      await tester.pumpAndSettle();
      await tester.enterText(
        find.widgetWithText(TextField, '模型名称'),
        'my-model',
      );
      await tester.scrollUntilVisible(
        find.widgetWithText(TextField, '服务地址'),
        180,
        scrollable: find
            .descendant(
              of: find.byType(ListView),
              matching: find.byType(Scrollable),
            )
            .first,
      );
      await tester.enterText(
        find.widgetWithText(TextField, '服务地址'),
        'http://127.0.0.1:9999/v1',
      );
      await tester.scrollUntilVisible(
        find.widgetWithText(TextField, 'API Key'),
        180,
        scrollable: find
            .descendant(
              of: find.byType(ListView),
              matching: find.byType(Scrollable),
            )
            .first,
      );
      await tester.enterText(
        find.widgetWithText(TextField, 'API Key'),
        'test-only-key',
      );
      await tester.ensureVisible(find.text('保存设置'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('保存设置'));
      await tester.pumpAndSettle();
      expect(saved?['model'], 'my-model');
      expect(saved?['api_key'], 'test-only-key');
      expect(saved?['enabled'], false);
      // The save acknowledgement briefly covers the bottom action row.
      await tester.pump(const Duration(seconds: 5));
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.text('测试已保存的模型'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('测试已保存的模型'));
      await tester.pumpAndSettle();
      expect(tested, isTrue);
      expect(find.text('连接成功，模型已响应。'), findsOneWidget);
      await dispose(tester);
    },
  );
  testWidgets('mobile panel stays inside viewport', (tester) async {
    tester.view.physicalSize = const Size(390, 844);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    final api = CormexApi(
      baseUrl: 'http://localhost',
      client: MockClient(
        (r) async => r.url.path == '/memory/search'
            ? json({'facts': []})
            : json(status()),
      ),
    );
    await tester.pumpWidget(app(api));
    await tester.pumpAndSettle();
    expect(tester.takeException(), isNull);
    expect(find.text('全量未开启'), findsOneWidget);
    await dispose(tester);
  });
}
