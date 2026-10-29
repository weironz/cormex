import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../api/api_exception.dart';
import '../models/archive_session.dart';
import 'connection_controller.dart';

/// 会话列表的状态。
class SessionsState {
  const SessionsState({
    this.sessions = const [],
    this.loading = false,
    this.error,
    this.agent,
    this.project,
    this.nextCursor,
    this.loadingMore = false,
  });

  final List<ArchiveSession> sessions;
  final bool loading;
  final String? error;

  /// 当前筛选的 agent。null = 不过滤。
  final String? agent;

  /// 当前筛选的项目。
  final String? project;

  /// 下一页的游标。null = 没有更多。
  final String? nextCursor;

  /// 正在追加下一页（与首次加载分开：首次加载要占满屏幕，追加不该把已有
  /// 的列表清空）。
  final bool loadingMore;

  bool get hasMore => nextCursor != null;

  SessionsState copyWith({
    List<ArchiveSession>? sessions,
    bool? loading,
    Object? error = _sentinel,
    Object? agent = _sentinel,
    Object? project = _sentinel,
    Object? nextCursor = _sentinel,
    bool? loadingMore,
  }) => SessionsState(
    sessions: sessions ?? this.sessions,
    loading: loading ?? this.loading,
    error: identical(error, _sentinel) ? this.error : error as String?,
    agent: identical(agent, _sentinel) ? this.agent : agent as String?,
    project: identical(project, _sentinel) ? this.project : project as String?,
    nextCursor: identical(nextCursor, _sentinel)
        ? this.nextCursor
        : nextCursor as String?,
    loadingMore: loadingMore ?? this.loadingMore,
  );
}

/// `copyWith` 里区分「没传」与「传了 null」。
const Object _sentinel = Object();

/// Riverpod 3 的 `Notifier`，不是 `StateNotifier`。
///
/// `StateNotifierProvider` 在 Riverpod 3 里已经移除；`Notifier` 是它的替代，
/// 而 `state` 是 `Notifier` 提供的可写字段。
class SessionsController extends Notifier<SessionsState> {
  @override
  SessionsState build() => const SessionsState();

  /// 首次加载（或换筛选条件后重新加载）。
  Future<void> load({String? agent, String? project}) async {
    state = state.copyWith(
      loading: true,
      error: null,
      agent: agent,
      project: project,
      sessions: const [],
      nextCursor: null,
    );
    try {
      final page = await ref
          .read(apiProvider)
          .listArchiveSessions(agent: agent, project: project);
      state = state.copyWith(
        sessions: page.sessions,
        loading: false,
        nextCursor: page.nextCursor,
      );
    } on CormexApiException catch (e) {
      state = state.copyWith(loading: false, error: e.message);
    }
  }

  /// 追加下一页。
  ///
  /// 游标为空时什么都不做：再请求一次会拿回同一页，而列表会重复 —— 那种
  /// 重复在滚动到底时看起来像「加载了两遍」。
  Future<void> loadMore() async {
    final cursor = state.nextCursor;
    if (cursor == null || state.loadingMore) return;

    state = state.copyWith(loadingMore: true);
    try {
      final page = await ref
          .read(apiProvider)
          .listArchiveSessions(
            agent: state.agent,
            project: state.project,
            cursor: cursor,
          );
      state = state.copyWith(
        sessions: [...state.sessions, ...page.sessions],
        loadingMore: false,
        nextCursor: page.nextCursor,
      );
    } on CormexApiException catch (e) {
      state = state.copyWith(loadingMore: false, error: e.message);
    }
  }
}

final sessionsControllerProvider =
    NotifierProvider<SessionsController, SessionsState>(SessionsController.new);
