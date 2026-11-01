import 'dart:async';

import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../api/api_exception.dart';
import '../models/memory_artifact.dart';
import 'connection_controller.dart';

class MemoryArtifactsState {
  const MemoryArtifactsState({
    this.items = const [],
    this.nextCursor,
    this.loading = false,
    this.hasLoaded = false,
    this.error,
    this.failedMore = false,
  });
  final List<MemoryArtifact> items;
  final String? nextCursor, error;
  final bool loading, hasLoaded, failedMore;
}

class MemoryArtifactsController
    extends Notifier<Map<MemoryArtifactKind, MemoryArtifactsState>> {
  int _generation = 0;
  MemoryArtifactKind? _activeKind;

  @override
  Map<MemoryArtifactKind, MemoryArtifactsState> build() {
    ref.listen(apiProvider, (_, _) {
      _generation++;
      state = {};
      if (_activeKind case final kind?) unawaited(load(kind));
    });
    return {};
  }

  void select(MemoryArtifactKind? kind) {
    _activeKind = kind;
    if (kind != null) unawaited(load(kind));
  }

  /// 新分析结果到达后只刷新正在阅读的类别，其余类别下次打开再取。
  void invalidate() {
    _generation++;
    state = {};
    if (_activeKind case final kind?) unawaited(load(kind));
  }

  Future<void> load(
    MemoryArtifactKind kind, {
    bool more = false,
    bool refresh = false,
  }) async {
    final before = state[kind] ?? const MemoryArtifactsState();
    if (before.loading ||
        (more && before.nextCursor == null) ||
        (!more && !refresh && before.hasLoaded)) {
      return;
    }
    final generation = _generation;
    final cursor = more ? before.nextCursor : null;
    state = {
      ...state,
      kind: MemoryArtifactsState(
        items: before.items,
        nextCursor: before.nextCursor,
        hasLoaded: before.hasLoaded,
        loading: true,
      ),
    };
    try {
      final page = await ref
          .read(apiProvider)
          .memoryArtifacts(kind: kind, cursor: cursor);
      if (!ref.mounted || generation != _generation) return;
      final byId = {
        if (more)
          for (final item in before.items) item.id: item,
        for (final item in page.items) item.id: item,
      };
      state = {
        ...state,
        kind: MemoryArtifactsState(
          items: List.unmodifiable(byId.values),
          nextCursor: page.nextCursor == cursor ? null : page.nextCursor,
          hasLoaded: true,
        ),
      };
    } on Object catch (e) {
      if (!ref.mounted || generation != _generation) return;
      state = {
        ...state,
        kind: MemoryArtifactsState(
          items: before.items,
          nextCursor: before.nextCursor,
          hasLoaded: before.hasLoaded,
          error: e is CormexApiException ? e.message : '$e',
          failedMore: more,
        ),
      };
    }
  }
}

final memoryArtifactsProvider =
    NotifierProvider<
      MemoryArtifactsController,
      Map<MemoryArtifactKind, MemoryArtifactsState>
    >(MemoryArtifactsController.new);
