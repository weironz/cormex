import 'package:cormex_app/features/sessions/sessions_page.dart';
import 'package:cormex_app/models/archive_session.dart';
import 'package:cormex_app/state/connection_controller.dart';
import 'package:cormex_app/state/sessions_controller.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';

/// 一个只返回固定页的控制器，不碰网络。
///
/// 继承真的 `SessionsController` 并覆盖两个方法，而不是另写一个接口：
/// 被测的是**页面**，它调用的正是这两个方法，接口一致就够了。
class _FakeSessions extends SessionsController {
  _FakeSessions(this.pages);

  final List<ArchiveSessionsPage> pages;
  int _call = 0;

  @override
  SessionsState build() => const SessionsState();

  @override
  Future<void> load({String? agent, String? project}) async {
    _call = 0;
    final page = pages.first;
    state = SessionsState(
      sessions: page.sessions,
      nextCursor: page.nextCursor,
      agent: agent,
    );
  }

  @override
  Future<void> loadMore() async {
    _call += 1;
    if (_call >= pages.length) return;
    final page = pages[_call];
    state = state.copyWith(
      sessions: [...state.sessions, ...page.sessions],
      nextCursor: page.nextCursor,
    );
  }
}

Widget _wrap(List<ArchiveSessionsPage> pages) => ProviderScope(
  overrides: [
    sessionsControllerProvider.overrideWith(() => _FakeSessions(pages)),
    // 页面读 apiProvider 只是为了刷新，这里给一个不会用到的覆盖
    apiProvider.overrideWith((ref) => throw UnimplementedError()),
  ],
  child: const MaterialApp(home: SessionsPage()),
);

void main() {
  testWidgets('列表显示会话，并标出唯一副本', (tester) async {
    await tester.pumpWidget(
      _wrap([
        const ArchiveSessionsPage(
          sessions: [
            ArchiveSession(
              id: '01S',
              agent: 'claude_code',
              title: '对象存储用什么',
              eventCount: 12,
            ),
            ArchiveSession(
              id: '02S',
              agent: 'codex',
              title: '宿主已删的那段',
              eventCount: 3,
              soleCopy: true,
            ),
          ],
        ),
      ]),
    );
    await tester.pumpAndSettle();

    expect(find.text('对象存储用什么'), findsOneWidget);
    expect(find.text('宿主已删的那段'), findsOneWidget);
    expect(
      find.byIcon(Icons.warning_amber_rounded),
      findsOneWidget,
      reason: '唯一副本必须标出来 —— 在这里删除就是删掉最后一份',
    );
  });

  testWidgets('空列表给出下一步', (tester) async {
    await tester.pumpWidget(
      _wrap([const ArchiveSessionsPage()]),
    );
    await tester.pumpAndSettle();
    expect(find.text('还没有采集到会话'), findsOneWidget);
    expect(
      find.textContaining('cormex init'),
      findsOneWidget,
      reason: '空状态要说下一步做什么，而不是只说「没有」',
    );
  });

  testWidgets('滚到底追加下一页', (tester) async {
    await tester.pumpWidget(
      _wrap([
        ArchiveSessionsPage(
          sessions: [
            for (var i = 0; i < 30; i++)
              ArchiveSession(id: '0$i', agent: 'claude_code', title: '会话 $i'),
          ],
          nextCursor: 'cursor-1',
        ),
        const ArchiveSessionsPage(
          sessions: [
            ArchiveSession(id: '99', agent: 'codex', title: '第二页的会话'),
          ],
        ),
      ]),
    );
    await tester.pumpAndSettle();
    expect(find.text('第二页的会话'), findsNothing);

    await tester.drag(find.byType(ListView), const Offset(0, -4000));
    await tester.pumpAndSettle();

    expect(
      find.text('第二页的会话'),
      findsOneWidget,
      reason: '滚到底要按游标取下一页；没有游标时再请求一次会拿回同一页，列表就重复了',
    );
  });
}
