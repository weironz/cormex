import 'dart:convert';

import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/features/sessions/session_detail_page.dart';
import 'package:cormex_app/features/sessions/widgets/event_tile.dart';
import 'package:cormex_app/state/reading_preferences.dart';
import 'package:cormex_app/state/connection_controller.dart' as connection;
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

class _Connection extends connection.ConnectionController {
  int refreshes = 0;
  @override
  connection.ConnectionState build() => const connection.ConnectionState();
  @override
  Future<bool> handleUnauthorized() async {
    refreshes++;
    return true;
  }
}

void main() {
  testWidgets('对话视图隐藏内部事件，过程视图可再次查看', (tester) async {
    final client = MockClient((request) async {
      if (!request.url.path.endsWith('/events')) {
        return http.Response(
          jsonEncode({
            'session': {
              'id': 'one',
              'agent': 'codex',
              'title': '对话',
              'source_archived': true,
            },
            'files': [],
          }),
          200,
          headers: {'content-type': 'application/json; charset=utf-8'},
        );
      }
      return http.Response(
        jsonEncode({
          'events': [
            {
              'id': '1',
              'seq': 1,
              'kind': 'user_message',
              'content': [
                {
                  'type': 'text',
                  'text': '<environment_context>internal</environment_context>',
                },
              ],
            },
            {
              'id': '2',
              'seq': 2,
              'kind': 'user_message',
              'content': [
                {'type': 'text', 'text': '用户提问'},
              ],
            },
            {
              'id': '3',
              'seq': 3,
              'kind': 'agent_message',
              'content': [
                {'type': 'text', 'text': '助手答复'},
              ],
            },
          ],
        }),
        200,
        headers: {'content-type': 'application/json; charset=utf-8'},
      );
    });
    final api = CormexApi(baseUrl: 'https://example.test/api', client: client);
    await tester.pumpWidget(
      ProviderScope(
        overrides: [connection.apiProvider.overrideWithValue(api)],
        child: const MaterialApp(home: SessionDetailPage(sessionId: 'one')),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('用户提问'), findsOneWidget);
    expect(find.text('来自已归档聊天'), findsOneWidget);
    expect(find.text('助手答复'), findsOneWidget);
    expect(find.text('聊天上下文'), findsNothing);
    await tester.tap(find.text('查看过程'));
    await tester.pumpAndSettle();
    expect(find.text('聊天准备信息'), findsOneWidget);
    expect(
      tester.getTopLeft(find.text('聊天准备信息')).dy,
      lessThan(tester.getTopLeft(find.text('用户提问')).dy),
    );
    await tester.tap(find.text('聊天准备信息'));
    await tester.pumpAndSettle();
    expect(find.text('聊天上下文'), findsOneWidget);
  });

  testWidgets('阅读宽度变化作用于消息卡片', (tester) async {
    tester.view.physicalSize = const Size(1800, 900);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    final client = MockClient((request) async {
      if (!request.url.path.endsWith('/events')) {
        return http.Response(
          '{"session":{"id":"one","agent":"codex"},"files":[]}',
          200,
        );
      }
      return http.Response(
        '{"events":[{"id":"1","seq":1,"kind":"user_message","content":[{"type":"text","text":"hello"}]}]}',
        200,
      );
    });
    final container = ProviderContainer(
      overrides: [
        connection.apiProvider.overrideWithValue(
          CormexApi(baseUrl: 'https://example.test', client: client),
        ),
      ],
    );
    addTearDown(container.dispose);
    await tester.pumpWidget(
      UncontrolledProviderScope(
        container: container,
        child: const MaterialApp(home: SessionDetailPage(sessionId: 'one')),
      ),
    );
    await tester.pumpAndSettle();
    container.read(readingPreferencesProvider.notifier).preview(600);
    await tester.pumpAndSettle();
    final narrow = tester.getSize(find.byType(EventTile)).width;
    container.read(readingPreferencesProvider.notifier).preview(1200);
    await tester.pumpAndSettle();
    final wide = tester.getSize(find.byType(EventTile)).width;
    expect(wide - narrow, closeTo(600, 1));
    final card = find
        .descendant(
          of: find.byType(EventTile),
          matching: find.byType(Container),
        )
        .first;
    expect(tester.getSize(card).width, greaterThan(960));
  });
  testWidgets(
    'session detail retries once after a successful credential refresh',
    (tester) async {
      var calls = 0;
      final controller = _Connection();
      final client = MockClient((request) async {
        // The memory panel's own status request is not what this counts
        if (request.url.path.endsWith('/memory')) {
          return http.Response('{"supported":false}', 200);
        }
        calls++;
        if (calls == 1) return http.Response('{"error":"expired"}', 401);
        if (request.url.path.endsWith('/events')) {
          return http.Response('{"events":[]}', 200);
        }
        return http.Response(
          jsonEncode({
            'session': {'id': 'one', 'agent': 'codex', 'title': 'Recovered'},
            'files': [],
          }),
          200,
        );
      });
      final api = CormexApi(
        baseUrl: 'https://example.test/api',
        client: client,
      );
      await tester.pumpWidget(
        ProviderScope(
          overrides: [
            connection.apiProvider.overrideWithValue(api),
            connection.connectionProvider.overrideWith(() => controller),
          ],
          child: const MaterialApp(home: SessionDetailPage(sessionId: 'one')),
        ),
      );
      await tester.pumpAndSettle();
      expect(controller.refreshes, 1);
      expect(calls, 3);
      expect(find.text('Recovered'), findsOneWidget);
    },
  );

  testWidgets('复制整段对话先取回剩下的页，再按说话人复制', (tester) async {
    tester.view.physicalSize = const Size(1400, 2400);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    String? copied;
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          copied = (call.arguments as Map)['text'] as String?;
        }
        return null;
      },
    );
    addTearDown(
      () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        null,
      ),
    );
    Map<String, Object?> message(int seq, String kind, String text) => {
      'id': '$seq',
      'seq': seq,
      'kind': kind,
      'content': [
        {'type': 'text', 'text': text},
      ],
    };
    final pages = <String>[];
    final client = MockClient((request) async {
      if (!request.url.path.endsWith('/events')) {
        return http.Response(
          jsonEncode({
            'session': {'id': 'one', 'agent': 'claude_code', 'title': '复制测试'},
            'files': [],
          }),
          200,
          headers: {'content-type': 'application/json; charset=utf-8'},
        );
      }
      final cursor = request.url.queryParameters['cursor'];
      pages.add(cursor ?? 'first');
      final body = cursor == null
          ? {
              // Enough to fill the screen, so scrolling has not fetched page 2
              'events': [
                for (var i = 0; i < 8; i++) ...[
                  message(i * 2, 'user_message', '问题 $i'),
                  message(i * 2 + 1, 'agent_message', '回答 $i'),
                ],
              ],
              'next_cursor': 'p2',
            }
          : {
              'events': [
                message(100, 'user_message', '最后的问题'),
                message(101, 'agent_message', '最后的回答'),
              ],
            };
      return http.Response(
        jsonEncode(body),
        200,
        headers: {'content-type': 'application/json; charset=utf-8'},
      );
    });
    final api = CormexApi(baseUrl: 'https://example.test/api', client: client);
    await tester.pumpWidget(
      ProviderScope(
        overrides: [connection.apiProvider.overrideWithValue(api)],
        child: const MaterialApp(home: SessionDetailPage(sessionId: 'one')),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.byType(SelectionArea), findsOneWidget);

    await tester.tap(find.byTooltip('复制整段对话'));
    await tester.pumpAndSettle();
    expect(
      pages,
      contains('p2'),
      reason: 'the rest of the conversation is fetched first',
    );
    expect(copied, isNotNull);
    expect(copied, startsWith('# 复制测试'));
    expect(copied, contains('**你**'));
    expect(copied, contains('问题 0'));
    expect(
      copied,
      contains('最后的回答'),
      reason: 'the copy holds the whole conversation, not the part scrolled to',
    );
    expect(
      copied!.indexOf('问题 0'),
      lessThan(copied!.indexOf('最后的问题')),
      reason: 'in order',
    );
    expect(find.text('已复制整段对话（18 条）'), findsOneWidget);
  });
}
