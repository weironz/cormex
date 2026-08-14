import 'dart:async';

import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../api/api_exception.dart';
import '../models/episode.dart';
import '../models/memory_search_result.dart';
import 'connection_controller.dart';

class MemoryState {
  const MemoryState({
    this.query = '',
    this.result = MemorySearchResult.empty,
    this.loading = false,
    this.error,
    this.asOf,
    this.hasSearched = false,
  });

  final String query;
  final MemorySearchResult result;
  final bool loading;
  final String? error;

  /// 系统时间上的回放点。null = 「现在」。
  final DateTime? asOf;

  final bool hasSearched;

  bool get isTimeTravelling => asOf != null;

  MemoryState copyWith({
    String? query,
    MemorySearchResult? result,
    bool? loading,
    Object? error = _sentinel,
    Object? asOf = _sentinel,
    bool? hasSearched,
  }) => MemoryState(
    query: query ?? this.query,
    result: result ?? this.result,
    loading: loading ?? this.loading,
    error: error == _sentinel ? this.error : error as String?,
    asOf: asOf == _sentinel ? this.asOf : asOf as DateTime?,
    hasSearched: hasSearched ?? this.hasSearched,
  );

  static const Object _sentinel = Object();
}

class MemoryController extends Notifier<MemoryState> {
  Timer? _debounce;

  /// 防一个慢的早请求盖掉一个快的晚请求。
  int _requestSeq = 0;

  @override
  MemoryState build() {
    ref.onDispose(() => _debounce?.cancel());

    // 换了后端或换了钥匙就重来 —— 两边的结果没有可比性。
    //
    // # 这里为什么要**作废在飞的请求**，而不只是清空
    //
    // 重建 `apiProvider` 会 dispose 掉旧的客户端，而那个 dispose 是
    // `_client.close()` —— **正在飞的请求会当场被掐断**，抛出
    // 「Connection closed before full header was received」。
    //
    // 那个异常在清空之后才落地，于是它把 `error` 写了回去，界面停在
    // 「检索失败」直到用户手点一次刷新。而这恰恰是最常见的一次：
    // access token 过期触发续期，钥匙一换客户端就重建 —— 每 15 分钟
    // 就有一次机会命中。
    //
    // 作废序号让那具尸体被当作「已被取代」丢掉（`search` 里那道
    // `seq != _requestSeq` 的闸门），清空才真的是清空。
    ref.listen(apiProvider, (_, _) {
      _requestSeq++;
      final searched = state.hasSearched;
      state = MemoryState(query: state.query, asOf: state.asOf);
      // 之前搜过就自动重来一次：用户没有做错任何事，不该由他来点刷新。
      // 续期成功之后走的正是这一条 —— 于是一次令牌过期在界面上什么都
      // 不会发生，而那正是应该发生的事。
      if (searched) unawaited(search());
    });
    return const MemoryState();
  }

  void setQuery(String query) {
    state = state.copyWith(query: query);
    _debounce?.cancel();
    _debounce = Timer(const Duration(milliseconds: 260), search);
  }

  /// 挪回放点。立刻重查：用户是在**对比两个时刻**，这里去抖会让人觉得坏了。
  void setAsOf(DateTime? asOf) {
    state = state.copyWith(asOf: asOf);
    search();
  }

  /// 查 [Ref.mounted] 与序号闸门是同一件事的两半：这个方法会从一个 `Timer`
  /// 回调里进来，而那个时刻跟这个 provider 的生命周期毫无关系。provider
  /// 已经 dispose 之后再写 state，异常会从一个没人 await 的 future 里抛出去。
  Future<void> search() async {
    _debounce?.cancel();
    if (!ref.mounted) return;
    final seq = ++_requestSeq;
    final query = state.query.trim();

    state = state.copyWith(loading: true, error: null, hasSearched: true);
    try {
      final result = await ref
          .read(apiProvider)
          .searchMemory(query, limit: 30, asOf: state.asOf);
      if (seq != _requestSeq || !ref.mounted) return; // 已被取代
      state = state.copyWith(result: result, loading: false);
    } on CormexApiException catch (e) {
      if (seq != _requestSeq || !ref.mounted) return;
      state = state.copyWith(loading: false, error: e.message);
    } on Object catch (e) {
      if (seq != _requestSeq || !ref.mounted) return;
      state = state.copyWith(loading: false, error: '$e');
    }
  }
}

final memoryControllerProvider =
    NotifierProvider<MemoryController, MemoryState>(MemoryController.new);

/// 单条事实的出处。
///
/// 用 family provider 而不是命令式地去取：几张卡片可以各自打开自己的
/// episode，缓存与去重交给 Riverpod。
final episodeProvider = FutureProvider.family<Episode, String>((ref, id) async {
  return ref.watch(apiProvider).episode(id);
});
