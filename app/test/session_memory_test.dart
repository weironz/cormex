import 'dart:convert';

import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/features/lookup/id_lookup.dart';
import 'package:cormex_app/features/memory/memory_browser.dart';
import 'package:cormex_app/features/memory/widgets/fact_card.dart';
import 'package:cormex_app/features/memory/widgets/fact_row.dart';
import 'package:cormex_app/features/sessions/session_detail_page.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

const session = '01JC7ZZZZZZZZZZZZZZZSESSN1';
const memory = '01JC7ZZZZZZZZZZZZZZZMEMRY1';
const episode = '01JC7ZZZZZZZZZZZZZZZEPSDE1';

http.Response json(Object body, [int status = 200]) => http.Response(
  jsonEncode(body),
  status,
  headers: {'content-type': 'application/json; charset=utf-8'},
);

Map<String, Object?> fact(String id, String statement) => {
  'id': id,
  'statement': statement,
  'predicate': 'describes',
  'created_at': '2026-11-01T08:00:00Z',
  'source_episode_id': episode,
  'invalidated': false,
};

/// Serves one archived session and its memory endpoints.
class Server {
  Map<String, Object?> status = {
    'supported': true,
    'pending_turns': 45,
    'max_turns_per_job': 30,
    'memory_count': 1,
    'job': null,
    'other_job_running': false,
  };
  final requests = <String>[];

  Future<http.Response> handle(http.Request r) async {
    requests.add('${r.method} ${r.url.path}');
    final path = r.url.path;
    if (path == '/archive/sessions/$session/memory') {
      if (r.method == 'POST') {
        status = {
          ...status,
          'job': {
            'id': 'job',
            'status': 'running',
            'total_messages': 30,
            'processed_messages': 4,
          },
        };
        return json({'job': status['job']});
      }
      return json(status);
    }
    if (path == '/archive/sessions/$session/memories') {
      return json({
        'session_id': session,
        'facts': [fact(memory, '项目选用 Rust。')],
      });
    }
    if (path == '/archive/sessions/$session/events') {
      return json({'events': []});
    }
    if (path == '/archive/sessions/$session') {
      return json({
        'session': {'id': session, 'agent': 'codex', 'title': '选型讨论'},
        'files': [],
      });
    }
    if (path == '/memory/context') return json({'contexts': []});
    if (path.startsWith('/lookup/')) {
      final id = path.split('/').last;
      if (id == session) {
        return json({'kind': 'session', 'id': session, 'session_id': session});
      }
      return json({'error': '没有找到这个 ID 对应的会话或记忆'}, 404);
    }
    if (path == '/memory/search') {
      return json({
        'facts': [fact(memory, '项目选用 Rust。'), fact('${memory}X', '其他')],
      });
    }
    return json({'enabled': false, 'phase': 'paused'});
  }
}

Widget app(Server server, Widget home) => ProviderScope(
  overrides: [
    apiProvider.overrideWithValue(
      CormexApi(baseUrl: 'http://localhost', client: MockClient(server.handle)),
    ),
  ],
  child: MaterialApp(home: home),
);

void main() {
  testWidgets(
    'a session shows its id and memory count, and generating asks first',
    (tester) async {
      final server = Server();
      await tester.pumpWidget(
        app(server, const SessionDetailPage(sessionId: session)),
      );
      await tester.pumpAndSettle();

      expect(find.byKey(const ValueKey('id-chip-$session')), findsOneWidget);
      expect(find.text('本会话记忆 1 条'), findsOneWidget);
      expect(find.text('继续生成记忆'), findsOneWidget);

      await tester.tap(find.byKey(const ValueKey('session-memory-generate')));
      await tester.pumpAndSettle();
      expect(
        find.textContaining('这次最多 30 轮'),
        findsOneWidget,
        reason: 'the confirmation states how much one click will analyse',
      );
      expect(find.textContaining('约还有 45 轮未分析'), findsOneWidget);
      expect(
        server.requests.where((r) => r.startsWith('POST')),
        isEmpty,
        reason: 'nothing is sent before the person confirms',
      );

      await tester.tap(find.byKey(const ValueKey('session-memory-confirm')));
      await tester.pump();
      await tester.pump();
      expect(
        server.requests,
        contains('POST /archive/sessions/$session/memory'),
      );
      expect(find.text('正在生成 4/30 轮'), findsOneWidget);

      // The job ends: the next poll reports it and the page says so
      server.status = {
        ...server.status,
        'pending_turns': 15,
        'memory_count': 6,
        'job': {
          'id': 'job',
          'status': 'completed',
          'total_messages': 30,
          'processed_messages': 30,
        },
      };
      await tester.pump(const Duration(seconds: 2));
      await tester.pump();
      expect(find.text('本会话记忆 6 条'), findsOneWidget);
      expect(find.textContaining('生成完成：分析了 30 轮'), findsOneWidget);
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets('a session lists the memories it produced', (tester) async {
    final server = Server();
    await tester.pumpWidget(
      app(server, const SessionDetailPage(sessionId: session)),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('session-memory-count')));
    await tester.pumpAndSettle();
    expect(find.text('本会话记忆（1）'), findsOneWidget);
    expect(find.byType(FactRow), findsOneWidget);
    expect(find.text('项目选用 Rust。'), findsOneWidget);
    expect(find.byKey(const ValueKey('id-chip-$memory')), findsOneWidget);
  });

  testWidgets('a caught-up session offers nothing to pay for', (tester) async {
    final server = Server()
      ..status = {
        'supported': true,
        'pending_turns': 0,
        'max_turns_per_job': 30,
        'memory_count': 3,
      };
    await tester.pumpWidget(
      app(server, const SessionDetailPage(sessionId: session)),
    );
    await tester.pumpAndSettle();
    expect(
      find.byKey(const ValueKey('session-memory-caught-up')),
      findsOneWidget,
    );
    expect(find.byKey(const ValueKey('session-memory-generate')), findsNothing);
  });

  testWidgets('an id opens what it names; a malformed one is refused locally', (
    tester,
  ) async {
    final server = Server();
    await tester.pumpWidget(
      app(server, const Scaffold(body: Center(child: IdLookupButton()))),
    );
    await tester.tap(find.byKey(const ValueKey('id-lookup')));
    await tester.pumpAndSettle();

    await tester.enterText(
      find.byKey(const ValueKey('id-lookup-input')),
      'abc',
    );
    await tester.tap(find.byKey(const ValueKey('id-lookup-submit')));
    await tester.pumpAndSettle();
    expect(find.textContaining('26 位'), findsWidgets);
    expect(server.requests.where((r) => r.contains('/lookup/')), isEmpty);

    await tester.enterText(
      find.byKey(const ValueKey('id-lookup-input')),
      '01JC7ZZZZZZZZZZZZZZZNPQE01',
    );
    await tester.tap(find.byKey(const ValueKey('id-lookup-submit')));
    await tester.pumpAndSettle();
    expect(find.textContaining('没有找到'), findsOneWidget);

    await tester.enterText(
      find.byKey(const ValueKey('id-lookup-input')),
      session,
    );
    await tester.tap(find.byKey(const ValueKey('id-lookup-submit')));
    await tester.pumpAndSettle();
    expect(find.byType(SessionDetailPage), findsOneWidget);
    expect(find.text('选型讨论'), findsOneWidget);
  });

  testWidgets('memories switch between cards and a list', (tester) async {
    tester.view.physicalSize = const Size(1400, 1000);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);
    final server = Server();
    await tester.pumpWidget(app(server, const MemoryBrowser()));
    await tester.pumpAndSettle();
    expect(find.byType(FactCard), findsNWidgets(2));
    expect(find.byType(FactRow), findsNothing);

    await tester.tap(find.byKey(const ValueKey('memory-view-toggle')));
    await tester.pumpAndSettle();
    expect(find.byType(FactRow), findsNWidgets(2));
    expect(find.byType(FactCard), findsNothing);
    expect(find.byKey(const ValueKey('id-chip-$memory')), findsOneWidget);

    await tester.tap(find.byKey(const ValueKey('memory-view-toggle')));
    await tester.pumpAndSettle();
    expect(find.byType(FactCard), findsNWidgets(2));
    await tester.pumpWidget(const SizedBox());
  });
}
