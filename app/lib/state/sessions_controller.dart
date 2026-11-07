import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../api/api_exception.dart';
import '../core/agents.dart';
import '../models/archive_session.dart';
import 'connection_controller.dart';

class ProjectChats {
  const ProjectChats({
    this.sessions = const [],
    this.loaded = false,
    this.loading = false,
    this.cursor,
    this.error,
  });
  final List<ArchiveSession> sessions;
  final bool loaded;
  final bool loading;
  final String? cursor;
  final String? error;
}

class SessionsState {
  const SessionsState({
    this.projects = const [],
    this.groups = const {},
    this.loading = false,
    this.error,
    this.agent,
    this.query = '',
    this.sources = const [],
  });
  final List<ArchiveProject> projects;

  /// 有会话的来源。取不到时为空 —— 筛选器只剩「全部来源」，列表照常可用。
  final List<ArchiveSource> sources;
  final Map<String?, ProjectChats> groups;
  final bool loading;
  final String? error;
  final String? agent;
  final String query;

  /// 来源的显示名：服务端给的名字，取不到时退回内置表。
  String sourceName(String agent) {
    for (final s in sources) {
      if (s.agent == agent) return s.name;
    }
    return Agents.label(agent);
  }
}

class SessionsController extends Notifier<SessionsState> {
  int _generation = 0;
  @override
  SessionsState build() => const SessionsState();

  Future<void> load({String? agent, String query = ''}) async {
    final generation = ++_generation;
    final keep = state.sources;
    state = SessionsState(
      loading: true,
      agent: agent,
      query: query.trim(),
      sources: keep,
    );
    // 来源与项目并行取；来源失败不拖垮页面，只是筛选器少了选项
    final sourcesFuture = ref
        .read(apiProvider)
        .archiveSources()
        .then<List<ArchiveSource>?>((v) => v)
        .catchError((Object _) => null);
    try {
      final projects = await ref
          .read(apiProvider)
          .archiveProjects(agent: agent, query: query.trim());
      final sources = await sourcesFuture ?? keep;
      if (!ref.mounted || generation != _generation) return;
      state = SessionsState(
        projects: projects,
        agent: agent,
        query: query.trim(),
        sources: sources,
      );
    } on CormexApiException catch (e) {
      final sources = await sourcesFuture ?? keep;
      if (!ref.mounted || generation != _generation) return;
      state = SessionsState(
        agent: agent,
        query: query.trim(),
        error: e.message,
        sources: sources,
      );
    }
  }

  Future<void> loadProject(String? key, {bool more = false}) async {
    final generation = _generation;
    if (state.loading || !state.projects.any((p) => p.key == key)) return;
    final old = state.groups[key] ?? const ProjectChats();
    if (old.loading ||
        (old.loaded && !more && old.error == null) ||
        (more && old.cursor == null)) {
      return;
    }
    final cursor = old.loaded ? old.cursor : null;
    _put(
      key,
      ProjectChats(
        sessions: old.sessions,
        loaded: old.loaded,
        cursor: cursor,
        loading: true,
      ),
    );
    try {
      final page = await ref
          .read(apiProvider)
          .listArchiveSessions(
            agent: state.agent,
            project: key,
            unassigned: key == null,
            query: state.query,
            cursor: cursor,
            limit: 30,
          );
      if (!ref.mounted || generation != _generation) return;
      if (cursor != null && page.nextCursor == cursor) {
        _put(
          key,
          ProjectChats(
            sessions: old.sessions,
            loaded: old.loaded,
            cursor: cursor,
            error: '分页游标未前进，请重试',
          ),
        );
        return;
      }
      final ids = old.sessions.map((s) => s.id).toSet();
      _put(
        key,
        ProjectChats(
          sessions: [
            ...old.sessions,
            ...page.sessions.where((s) => ids.add(s.id)),
          ],
          loaded: true,
          cursor: page.nextCursor,
        ),
      );
    } on CormexApiException catch (e) {
      if (!ref.mounted || generation != _generation) return;
      _put(
        key,
        ProjectChats(
          sessions: old.sessions,
          loaded: old.loaded,
          cursor: cursor,
          error: e.message,
        ),
      );
    }
  }

  void _put(String? key, ProjectChats group) {
    state = SessionsState(
      projects: state.projects,
      groups: {...state.groups, key: group},
      agent: state.agent,
      query: state.query,
      sources: state.sources,
    );
  }
}

final sessionsControllerProvider =
    NotifierProvider<SessionsController, SessionsState>(SessionsController.new);
