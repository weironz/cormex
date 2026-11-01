class MemoryContext {
  const MemoryContext({
    required this.factId,
    required this.subject,
    required this.sessionId,
    required this.sourceRole,
    this.agent,
    this.chatTitle,
    this.projectName,
    this.sourceTime,
    this.reason,
    this.verification,
    this.extractionUnit = 'input_record',
  });
  final String factId, subject, sessionId, sourceRole, extractionUnit;
  final String? agent, chatTitle, projectName, reason, verification;
  final DateTime? sourceTime;
  factory MemoryContext.fromJson(Map<String, dynamic> j) => MemoryContext(
    factId: j['fact_id'] as String,
    subject: j['subject'] as String? ?? '记忆',
    sessionId: j['session_id'] as String,
    sourceRole: j['source_role'] as String? ?? '',
    agent: j['agent'] as String?,
    chatTitle: j['chat_title'] as String?,
    projectName: j['project_name'] as String?,
    reason: j['reason'] as String?,
    verification: j['verification'] as String?,
    sourceTime: DateTime.tryParse(j['source_time'] as String? ?? '')?.toLocal(),
    extractionUnit: j['extraction_unit'] as String? ?? 'input_record',
  );
  String get agentLabel => switch (agent) {
    'codex' => 'Codex 编程',
    'claude_code' => 'Claude Code',
    'chatgpt_export' => 'ChatGPT',
    'gemini' => 'Gemini',
    null => '外部输入',
    _ => agent!,
  };
  String get roleLabel => memorySourceRoleLabel(sourceRole);
  String get verificationLabel => switch (verification) {
    'evidence_supported' => '归档工具佐证',
    'reported' => '来源报告',
    'user_stated' => '原对话给定',
    _ => sourceRole == 'assistant' ? '来源报告' : '依据原对话',
  };
  String? get verificationDescription => switch (verification) {
    'evidence_supported' => '原对话包含归档的工具或测试记录。',
    'reported' => '来源报告，未经独立验证。',
    'user_stated' => '来自原对话给定的信息。',
    _ => sourceRole == 'assistant' ? '来源报告，未经独立验证。' : null,
  };
}

String memorySourceRoleLabel(String role) => switch (role) {
  'mixed' => '完整问答',
  'assistant' => 'AI 答复',
  'user' => '用户角色输入',
  'legacy' => '旧策略来源',
  '' => '输入记录',
  _ => role,
};

String memoryCategory(String? p) => switch (p) {
  final String predicate when predicate.startsWith('memory/') => '项目记忆',
  'prefers' => '偏好',
  'decided' => '方案记录',
  'requires' => '约束',
  'uses' => '使用方式',
  'owns' => '归属',
  'blocked_by' => '待解决事项',
  'deadline' => '时间约定',
  _ => '背景事实',
};
String memoryPurpose(String? p) => switch (p) {
  final String predicate when predicate.startsWith('memory/') =>
    '为后续相关任务提供项目结论、适用场景和已有背景。',
  'prefers' => '为后续交互提供偏好参考，减少重复说明。',
  'decided' => '为后续任务提供已有方案与决策背景。',
  'requires' => '为后续实施提供约束参考，避免遗漏已有要求。',
  'blocked_by' => '帮助后续任务了解问题背景与已有进展。',
  _ => '在后续相关问题中作为背景检索使用。',
};
