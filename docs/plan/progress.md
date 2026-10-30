# 会话档案 —— 开发进度

> 规则见 [session-archive-plan.md](session-archive-plan.md) §12。每完成一张卡更新一行。
> 会话被压缩或重启后，**先读这个文件**，从第一张不是 `done` 的卡继续。
>
> 状态：`todo` / `doing` / `done` / `blocked`。`blocked` 必须写明缺什么。

| 卡 | 标题 | 状态 | 提交 | 备注 |
|---|---|---|---|---|
| P0-2 | 认证主体一次解析（Principal） | todo | | |
| P0-1 | 空库启动修复 | todo | | |
| P0-4 | `cortex-proto` 的 `llm` feature | todo | | |
| P0-3 | `as_of` 回放忽略 revoke | todo | | |
| P0-5 | 文档纠错 | todo | | |
| P1-2 | 归档表 migration（R 层） | todo | | |
| P1-4 | 对象存储的 keyed 接口 | todo | | |
| P1-5 | 线协议与来源目录 | todo | | |
| P1-3 | `ArchiveTxn` 与归档模型 | todo | | |
| P1-1 | 设备密钥 | todo | | |
| P1-6 | 服务端采集接口 | todo | | |
| P1-7 | CLI 骨架 | todo | | |
| P1-8 | CLI 发现 | todo | | |
| P1-9 | CLI 同步引擎 | todo | | |
| P1-10 | hook 处理器与安装器 + 定时任务 | todo | | |
| P1-11 | `init` / `doctor` / `uninstall` | todo | | |
| P1-12 | 端到端测试 | todo | | |
| P1-13 | CLI 发布 | todo | | |
| P2-3 | 密钥区间扫描 | todo | | |
| P2-0 | 本机采样与结构保持脱敏 | todo | | |
| P2-1 | `cortex-transcript` 与两个解析器 | todo | | |
| P2-2 | P 层 migration 与存储方法 | todo | | |
| P2-4 | 后台 worker：masker 与 projector | todo | | |
| P2-5 | 浏览与原文 API | todo | | |
| P2-7 | CLI `sessions ls / show` | todo | | |
| P2-6 | Web 会话视图 | todo | | |
| P3-1 | rmcp 3.x + goose-providers 升级 | todo | | |
| P3-2 | 会话检索与原文档案索引 | todo | | |
| P3-3 | 会话的注入渲染（两个仓库） | todo | | |
| P3-4 | MCP 会话工具与资源 | todo | | |
| P3-5 | 导出与 `cormex restore` | todo | | |
| P3-6 | `cormex mcp` stdio 桥 | todo | | |
| P3-7 | MCP 注册、Agent Skill 与文档 | todo | | |
| P4-0 | 快照批量接口 + SQLite 行来源 | todo | | |
| P4-1 | Gemini CLI | todo | | |
| P4-2 | Cursor | todo | | |
| P4-3 | VS Code Copilot Chat | todo | | |
| P4-4 | Cline / Roo Code | todo | | |
| P4-5 | OpenCode、Goose | todo | | |
| P4-6 | Copilot CLI、Qwen Code、Aider | todo | | |
| P4-7 | 导出文件进 R 层 | todo | | |
| P5-1 | 派生边扩展 | todo | | |
| P5-2 | 观察日志 | todo | | |
| P5-3 | 交接简报 | todo | | |
| P5-4 | 按策略晋升与事实抽取 | todo | | |
| P5-5 | 事件时间轴 `at` | todo | | |
| P5-6 | 对话轮向量（默认关闭） | todo | | |
| P6-1 | 每来源数据密钥 | todo | | |
| P6-2 | purge 覆盖 R 层 | todo | | |
| P6-3 | 备份恢复演练 | todo | | |
| P6-4 | 设备码登录 + Web 设备页 | todo | | |
| P6-5 | npm 分发 | todo | | |
| P7-1 | 无数据库的采集测试进 CI | todo | | |
| P7-2 | 会话检索评测集 | todo | | |
| P7-3 | LongMemEval-S 检索召回 | todo | | |

## 偏离设计

（按 §12.4 记录：哪张卡、偏离了什么、证据、怎么做的、设计文档改了哪一段）

## 总结

（全部完成后按 §12.7 填写）
