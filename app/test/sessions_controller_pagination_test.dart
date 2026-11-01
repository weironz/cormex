import 'dart:async';
import 'dart:convert';
import 'package:cormex_app/api/cormex_api.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:cormex_app/state/sessions_controller.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';

http.Response response(Object value) => http.Response(
  jsonEncode(value),
  200,
  headers: {'content-type': 'application/json; charset=utf-8'},
);
ProviderContainer containerFor(
  Future<http.Response> Function(http.Request) handler,
) => ProviderContainer(
  overrides: [
    apiProvider.overrideWithValue(
      CormexApi(baseUrl: 'http://localhost', client: MockClient(handler)),
    ),
  ],
);

void main() {
  test(
    'loads only complete project facets until a project is expanded',
    () async {
      var chats = 0;
      final c = containerFor((r) async {
        if (r.url.path.endsWith('/projects')) {
          return response({
            'projects': [
              {'key': 'late', 'name': 'Last project', 'chat_count': 900},
              {'key': null, 'chat_count': 500},
            ],
          });
        }
        chats++;
        return response({'sessions': []});
      });
      addTearDown(c.dispose);
      await c.read(sessionsControllerProvider.notifier).load();
      expect(c.read(sessionsControllerProvider).projects.length, 2);
      expect(chats, 0);
      expect(c.read(sessionsControllerProvider).groups, isEmpty);
    },
  );
  test(
    'project pages are bounded and unassigned is explicit; reopening is cached',
    () async {
      final requests = <http.Request>[];
      final c = containerFor((r) async {
        requests.add(r);
        if (r.url.path.endsWith('/projects')) {
          return response({
            'projects': [
              {'key': null, 'chat_count': 60},
            ],
          });
        }
        expect(r.url.queryParameters['unassigned'], 'true');
        expect(r.url.queryParameters['limit'], '30');
        return response({
          'sessions': [
            {
              'id': r.url.queryParameters['cursor'] == null ? 'one' : 'two',
              'agent': 'codex',
            },
          ],
          if (r.url.queryParameters['cursor'] == null) 'next_cursor': 'next',
        });
      });
      addTearDown(c.dispose);
      final n = c.read(sessionsControllerProvider.notifier);
      await n.load();
      await n.loadProject(null);
      await n.loadProject(null);
      expect(requests.length, 2);
      expect(
        c.read(sessionsControllerProvider).groups[null]!.sessions.length,
        1,
      );
      await n.loadProject(null, more: true);
      expect(
        c
            .read(sessionsControllerProvider)
            .groups[null]!
            .sessions
            .map((s) => s.id),
        ['one', 'two'],
      );
      expect(requests.last.url.queryParameters['cursor'], 'next');
    },
  );
  test('late responses cannot contaminate another agent or search', () async {
    final pending = Completer<http.Response>();
    final requested = Completer<void>();
    final c = containerFor((r) async {
      if (r.url.path.endsWith('/projects')) {
        return response({
          'projects': [
            {'key': 'p', 'name': 'Project', 'chat_count': 100},
          ],
        });
      }
      requested.complete();
      return pending.future;
    });
    addTearDown(c.dispose);
    final n = c.read(sessionsControllerProvider.notifier);
    await n.load(agent: 'claude_code');
    final old = n.loadProject('p');
    await requested.future;
    await n.load(agent: 'codex', query: 'new');
    pending.complete(
      response({
        'sessions': [
          {'id': 'old', 'agent': 'claude_code'},
        ],
      }),
    );
    await old;
    expect(c.read(sessionsControllerProvider).agent, 'codex');
    expect(c.read(sessionsControllerProvider).query, 'new');
    expect(c.read(sessionsControllerProvider).groups, isEmpty);
  });
  test(
    'search is applied to facets and every project page before pagination',
    () async {
      final c = containerFor((r) async {
        expect(r.url.queryParameters['q'], 'needle');
        if (r.url.path.endsWith('/projects')) {
          return response({
            'projects': [
              {'key': 'p', 'chat_count': 1},
            ],
          });
        }
        expect(r.url.queryParameters['project'], 'p');
        return response({
          'sessions': [
            {'id': 'one', 'agent': 'codex'},
          ],
        });
      });
      addTearDown(c.dispose);
      final n = c.read(sessionsControllerProvider.notifier);
      await n.load(query: ' needle ');
      await n.loadProject('p');
      expect(
        c.read(sessionsControllerProvider).groups['p']!.sessions.length,
        1,
      );
    },
  );
  test(
    'page failure preserves earlier chats and retries the same cursor',
    () async {
      var failed = false;
      final c = containerFor((r) async {
        if (r.url.path.endsWith('/projects')) {
          return response({
            'projects': [
              {'key': 'p', 'chat_count': 31},
            ],
          });
        }
        if (r.url.queryParameters['cursor'] == null) {
          return response({
            'sessions': [
              {'id': 'one', 'agent': 'codex'},
            ],
            'next_cursor': 'next',
          });
        }
        if (!failed) {
          failed = true;
          return http.Response('{"error":"temporary"}', 503);
        }
        return response({
          'sessions': [
            {'id': 'two', 'agent': 'codex'},
          ],
        });
      });
      addTearDown(c.dispose);
      final n = c.read(sessionsControllerProvider.notifier);
      await n.load();
      await n.loadProject('p');
      await n.loadProject('p', more: true);
      expect(
        c.read(sessionsControllerProvider).groups['p']!.sessions.length,
        1,
      );
      expect(c.read(sessionsControllerProvider).groups['p']!.error, isNotNull);
      await n.loadProject('p', more: true);
      expect(
        c.read(sessionsControllerProvider).groups['p']!.sessions.length,
        2,
      );
    },
  );
}
