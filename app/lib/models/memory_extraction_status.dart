class MemoryExtractionStatus {
  const MemoryExtractionStatus({
    required this.enabled,
    required this.useServerModel,
    required this.provider,
    required this.model,
    required this.phase,
    this.baseUrl,
    this.keyConfigured = false,
    this.keyTail,
    this.modelConfigured = false,
    this.providers = const [],
    this.totalChats = 0,
    this.processedChats = 0,
    this.pendingChats = 0,
    this.factsCreated = 0,
    this.memoryCount,
    this.summaryCount,
    this.lastError,
    this.updatedAt,
    this.trial,
    this.since,
  });
  final bool enabled, useServerModel, keyConfigured, modelConfigured;
  final String provider, model, phase;
  final String? baseUrl, keyTail, lastError, updatedAt;
  final List<String> providers;
  final int totalChats, processedChats, pendingChats, factsCreated;
  final int? memoryCount, summaryCount;
  final Map<String, dynamic>? trial;

  /// 提取范围：只处理这个时间之后有过对话的聊天（RFC 3339）。`null` 是全部。
  final String? since;

  /// 「2026-11-01 之后」，或 `null`（全部聊天）。
  String? get sinceLabel {
    final t = since == null ? null : DateTime.tryParse(since!)?.toLocal();
    if (t == null) return null;
    String two(int v) => v.toString().padLeft(2, '0');
    return '${t.year}-${two(t.month)}-${two(t.day)} 之后';
  }

  bool get trialRunning => trial?['status'] == 'running';
  bool get hasAnalyzedChats =>
      processedChats > 0 ||
      ((trial?['processed_chats'] as num?)?.toInt() ?? 0) > 0;
  int? get trialMemoryCount => (trial?['memory_count'] as num?)?.toInt();
  bool get trialCompletedWithNoMemories =>
      trial?['status'] == 'completed' && trialMemoryCount == 0;

  List<String> get outputCountLabels => [
    '可用记忆 ${memoryCount ?? '—'}',
    '会话摘要 ${summaryCount ?? '—'}',
  ];

  List<String> get trialOutputCountLabels {
    final j = trial;
    if (j == null) return [];
    return [
      '自动保留记忆 ${trialMemoryCount ?? '—'}',
      '实际写入 ${(j['facts_created'] as num?)?.toInt() ?? '—'}',
      '会话摘要 ${(j['summary_count'] as num?)?.toInt() ?? '—'}',
    ];
  }

  factory MemoryExtractionStatus.fromJson(Map<String, dynamic> j) =>
      MemoryExtractionStatus(
        enabled: j['enabled'] == true,
        useServerModel: j['use_server_model'] != false,
        provider: j['provider'] as String? ?? '',
        model: j['model'] as String? ?? '',
        phase: j['phase'] as String? ?? 'unknown',
        baseUrl: j['base_url'] as String?,
        keyConfigured: j['key_configured'] == true,
        keyTail: j['key_tail'] as String?,
        modelConfigured: j['model_configured'] == true,
        providers: (j['providers'] as List? ?? []).whereType<String>().toList(),
        totalChats: (j['total_chats'] as num?)?.toInt() ?? 0,
        processedChats: (j['processed_chats'] as num?)?.toInt() ?? 0,
        pendingChats: (j['pending_chats'] as num?)?.toInt() ?? 0,
        factsCreated: (j['facts_created'] as num?)?.toInt() ?? 0,
        memoryCount: ((j['memory_count'] ?? j['active_facts']) as num?)
            ?.toInt(),
        summaryCount: (j['summary_count'] as num?)?.toInt(),
        lastError: j['last_error'] as String?,
        updatedAt: j['updated_at'] as String?,
        trial: (j['trial'] as Map?)?.cast<String, dynamic>(),
        since: j['since'] as String?,
      );
  String get label => switch (phase) {
    'paused' => '全量未开启',
    'needs_configuration' => '需要配置模型',
    'error' => '提取失败',
    'processing' => '正在提取',
    'caught_up' => '处理完成',
    'queued' => '等待处理',
    'trial_running' => '小样本提取中',
    _ => '状态未知',
  };

  /// 保存请求。范围必须原样带上：服务端把「没带 since」读作「全部聊天」，
  /// 只拨一下开关就会悄悄把用户选的范围放宽成全量。
  Map<String, Object?> settings({bool? enabled}) => {
    'enabled': enabled ?? this.enabled,
    'use_server_model': useServerModel,
    'provider': provider,
    'model': model,
    'base_url': baseUrl,
    'since': since,
  };
}
