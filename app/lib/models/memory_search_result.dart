import 'json.dart';
import 'memory_fact.dart';

/// 一条事实是被哪几路召回捞出来的，以及融合之后的分数。
///
/// cormexd 并行跑 BM25、向量、图、时近几路再用 RRF 融合；把每条命中的通道
/// 列出来，是「这条为什么会回来」在界面上答得出的唯一办法 —— 否则检索就是
/// 一个黑盒配一个相似度分数，而那正是别家给的东西。
class RetrievalChannels {
  const RetrievalChannels({
    required this.factId,
    required this.channels,
    this.score,
  });

  final String factId;

  /// 例如 `['bm25', 'vector']`。两路都在意味着它们彼此印证。
  final List<String> channels;

  /// 融合后的相关性分数。
  final double? score;

  factory RetrievalChannels.fromJson(Map<String, dynamic> json) =>
      RetrievalChannels(
        factId: asString(json['fact_id']),
        channels: asStringList(json['channels']),
        score: asDoubleOrNull(json['score']),
      );
}

/// `GET /memory/search` 的回体。
class MemorySearchResult {
  const MemorySearchResult({required this.facts, this.channels = const {}});

  final List<MemoryFact> facts;

  /// 按 fact id 索引，拼结果列表时是 O(1) 查表。
  final Map<String, RetrievalChannels> channels;

  static const empty = MemorySearchResult(facts: []);

  bool get isEmpty => facts.isEmpty;

  /// 这一屏里有多少条是「当时成立、现在已被推翻」的。
  ///
  /// 只有回放路径会大于 0（见 [MemoryFact.invalidated]），所以它同时也是
  /// 「这次回放到底有没有捞出跟现状不同的东西」这个问题的答案 —— 而那正是
  /// 用户拖那根滑块想看的。
  int get invalidatedCount => facts.where((f) => f.invalidated).length;

  factory MemorySearchResult.fromJson(Map<String, dynamic> json) {
    final channels = <String, RetrievalChannels>{};
    for (final raw in asObjectList(json['channels'])) {
      final c = RetrievalChannels.fromJson(raw);
      if (c.factId.isNotEmpty) channels[c.factId] = c;
    }
    return MemorySearchResult(
      facts: asObjectList(
        json['facts'],
      ).map(MemoryFact.fromJson).toList(growable: false),
      channels: channels,
    );
  }
}
