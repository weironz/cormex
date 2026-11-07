import 'package:flutter/material.dart';

/// 来源 agent 的显示名与标识色。
///
/// 一处定义：列表、详情、筛选器以前各写一份 `switch`，于是同一个来源在
/// 列表里叫「claude_code」、在详情里叫「Claude」—— 用户看到的是两个东西。
abstract final class Agents {
  /// 人认得的名字，在服务端的来源列表取不到时用。没见过的来源原样显示 id，
  /// 而不是猜一个。
  static String label(String agent) => switch (agent) {
    'claude_code' => 'Claude Code',
    'codex' => 'Codex',
    'chatgpt_export' => 'ChatGPT',
    'claude_export' => 'Claude',
    'gemini_cli' => 'Gemini CLI',
    'cursor' => 'Cursor',
    'opencode' => 'OpenCode',
    'copilot_cli' => 'Copilot CLI',
    'copilot_chat' => 'Copilot Chat',
    'qwen_code' => 'Qwen Code',
    'cline' => 'Cline',
    'roo_code' => 'Roo Code',
    'aider' => 'Aider',
    'goose' => 'goose',
    'pi' => 'pi',
    'omp' => 'omp',
    'deepseek_harness' => 'DeepSeek Harness',
    'hermes' => 'Hermes Agent',
    'grok_build' => 'Grok Build',
    'gemini_takeout' => 'Gemini',
    'grok_export' => 'Grok',
    _ => agent,
  };

  /// 对话里 AI 一方的称呼。
  static String speaker(String? agent) => switch (agent) {
    'codex' => 'Codex',
    'claude_code' || 'claude_export' => 'Claude',
    'gemini_cli' || 'gemini_takeout' => 'Gemini',
    'grok_build' || 'grok_export' => 'Grok',
    'deepseek_harness' => 'DeepSeek',
    'hermes' => 'Hermes',
    'pi' => 'pi',
    'omp' => 'omp',
    'chatgpt_export' => 'ChatGPT',
    _ => 'AI 助手',
  };

  /// 列表里那个小圆点的颜色。只用来区分来源，不承载状态，所以刻意压低饱和度：
  /// 一屏几十个点不能比标题还抢眼。
  static Color dot(String agent, Brightness brightness) {
    final hue = switch (agent) {
      'claude_code' || 'claude_export' => 24.0,
      'codex' || 'chatgpt_export' => 158.0,
      'gemini_cli' => 215.0,
      'cursor' => 265.0,
      'copilot_cli' || 'copilot_chat' => 290.0,
      'grok_build' || 'grok_export' => 0.0,
      'deepseek_harness' => 228.0,
      'pi' || 'omp' => 45.0,
      'hermes' => 190.0,
      _ => (agent.hashCode % 360).toDouble(),
    };
    return HSLColor.fromAHSL(
      1,
      hue,
      0.45,
      brightness == Brightness.dark ? 0.62 : 0.46,
    ).toColor();
  }
}
