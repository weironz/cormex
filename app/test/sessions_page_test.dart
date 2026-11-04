import 'dart:convert';
import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/features/sessions/sessions_page.dart';
import 'package:cormex_app/models/archive_session.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:cormex_app/state/sessions_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

http.Response response(Object value) => http.Response(
  jsonEncode(value),
  200,
  headers: {'content-type': 'application/json; charset=utf-8'},
);
Widget wrap(ProviderContainer c) => UncontrolledProviderScope(
  container: c,
  child: const MaterialApp(home: SessionsPage()),
);
ProviderContainer apiContainer(
  Future<http.Response> Function(http.Request) f,
) => ProviderContainer(
  overrides: [
    apiProvider.overrideWithValue(
      CormexApi(baseUrl: 'http://localhost', client: MockClient(f)),
    ),
  ],
);

void main() {
  testWidgets('short project labels preserve the full key for filtering', (
    tester,
  ) async {
    const projectKey = 'github.com/u/cormex';
    Uri? requested;
    final c = apiContainer((r) async {
      if (r.url.path.endsWith('/projects')) {
        return response({
          'projects': [
            {'key': projectKey, 'chat_count': 7},
          ],
        });
      }
      requested = r.url;
      return response({'sessions': []});
    });
    addTearDown(c.dispose);
    await tester.pumpWidget(wrap(c));
    await tester.pumpAndSettle();
    expect(find.text('cormex'), findsOneWidget);
    expect(find.byTooltip(projectKey), findsOneWidget);
    await tester.tap(find.text('cormex'));
    await tester.pumpAndSettle();
    expect(requested!.queryParameters['project'], projectKey);
  });
  test('older responses do not invent source tags or message counts', () {
    final s = ArchiveSession.fromJson({'id': 'one', 'agent': 'codex'});
    expect(s.sourceArchived, isFalse);
    expect(s.messageCount, isNull);
  });
  testWidgets(
    'project directory does not fetch chats; expanded pages show provenance and message counts',
    (tester) async {
      var calls = 0;
      final c = apiContainer((r) async {
        if (r.url.path.endsWith('/projects')) {
          return response({
            'projects': [
              {'key': null, 'chat_count': 896},
            ],
          });
        }
        calls++;
        return response({
          'sessions': [
            {
              'id': 'one',
              'agent': 'codex',
              'title': '旧聊天',
              'source_archived': true,
              'message_count': 2,
              'event_count': 947,
              'sole_copy': true,
            },
          ],
        });
      });
      addTearDown(c.dispose);
      await tester.pumpWidget(wrap(c));
      await tester.pumpAndSettle();
      expect(calls, 0);
      expect(find.text('896 个聊天'), findsOneWidget);
      expect(find.text('旧聊天'), findsNothing);
      await tester.tap(find.text('未分类'));
      await tester.pumpAndSettle();
      expect(calls, 1);
      expect(find.text('旧聊天'), findsOneWidget);
      expect(find.textContaining('2 条消息'), findsOneWidget);
      expect(find.textContaining('947'), findsNothing);
      expect(find.text('来自已归档聊天'), findsOneWidget);
      expect(find.byIcon(Icons.warning_amber_rounded), findsOneWidget);
      await tester.tap(find.text('未分类'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('未分类'));
      await tester.pumpAndSettle();
      expect(calls, 1);
    },
  );
  testWidgets('a large project fetches thirty chats per explicit page', (
    tester,
  ) async {
    var calls = 0;
    final c = apiContainer((r) async {
      if (r.url.path.endsWith('/projects')) {
        return response({
          'projects': [
            {'key': 'p', 'name': '大型项目', 'chat_count': 896},
          ],
        });
      }
      calls++;
      final start = r.url.queryParameters['cursor'] == null ? 0 : 30;
      return response({
        'sessions': [
          for (var i = start; i < start + 30; i++)
            {'id': '$i', 'agent': 'claude_code', 'title': '聊天 $i'},
        ],
        'next_cursor': start == 0 ? 'next' : null,
      });
    });
    addTearDown(c.dispose);
    await tester.pumpWidget(wrap(c));
    await tester.pumpAndSettle();
    await tester.tap(find.text('大型项目'));
    await tester.pumpAndSettle();
    expect(calls, 1);
    expect(c.read(sessionsControllerProvider).groups['p']!.sessions.length, 30);
    await tester.ensureVisible(find.text('加载更多聊天'));
    await tester.pumpAndSettle();
    expect(calls, 1);
    await tester.tap(find.text('加载更多聊天'));
    await tester.pumpAndSettle();
    expect(calls, 2);
    expect(c.read(sessionsControllerProvider).groups['p']!.sessions.length, 60);
  });
  testWidgets('search is debounced and sent to the server', (tester) async {
    final queries = <String?>[];
    final c = apiContainer((r) async {
      queries.add(r.url.queryParameters['q']);
      return response({
        'projects': [
          {'key': 'p', 'name': '远处项目', 'chat_count': 1},
        ],
      });
    });
    addTearDown(c.dispose);
    await tester.pumpWidget(wrap(c));
    await tester.pumpAndSettle();
    await tester.enterText(find.byType(TextField), 'needle');
    await tester.pump(const Duration(milliseconds: 301));
    await tester.pumpAndSettle();
    expect(queries, [null, 'needle']);
    expect(find.text('远处项目'), findsOneWidget);
  });
  testWidgets('narrow screens keep the project directory inside the viewport', (
    tester,
  ) async {
    tester.view.physicalSize = const Size(390, 844);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    final c = apiContainer(
      (r) async => response({
        'projects': [
          {'key': 'p', 'name': '项目', 'chat_count': 50},
        ],
      }),
    );
    addTearDown(c.dispose);
    await tester.pumpWidget(wrap(c));
    await tester.pumpAndSettle();
    expect(find.text('项目'), findsOneWidget);
    expect(find.text('选择聊天'), findsNothing);
    expect(tester.takeException(), isNull);
  });
}
