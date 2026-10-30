# 会话档案 —— 开发进度

> 规则见 [session-archive-plan.md](session-archive-plan.md) §12。每完成一张卡更新一行。
> 会话被压缩或重启后，**先读这个文件**，从第一张不是 `done` 的卡继续。
>
> 状态：`todo` / `doing` / `done` / `blocked`。`blocked` 必须写明缺什么。

| 卡 | 标题 | 状态 | 提交 | 备注 |
|---|---|---|---|---|
| P0-2 | 认证主体一次解析（Principal） | done | | Principal 一次解析；租户隔离测试 |
| P0-1 | 空库启动修复 | done | | 空库与升级两条路径都有测试 |
| P0-4 | `cormex-proto` 的 `llm` feature | done | | `cormex-proto` 的 llm feature |
| P0-3 | `as_of` 回放忽略 revoke | done | | `as_of` 快照谓词；lifecycle 测试 |
| P0-5 | 文档纠错 | done | | 文档纠错 |
| P1-2 | 归档表 migration（R 层） | done | | R 层 migration |
| P1-4 | 对象存储的 keyed 接口 | done | | keyed 接口 |
| P1-5 | 线协议与来源目录 | done | | 线协议与 agents.toml 目录 |
| P1-3 | `ArchiveTxn` 与归档模型 | done | | `ArchiveTxn`（锁 4273） |
| P1-1 | 设备密钥 | done | | 设备密钥 + 按设备吊销 |
| P1-6 | 服务端采集接口 | done | | `/capture/*` |
| P1-7 | CLI 骨架 | done | | CLI 骨架 |
| P1-8 | CLI 发现 | done | | catalog 展开 + detect |
| P1-9 | CLI 同步引擎 | done | | 同步引擎 |
| P1-10 | hook 处理器与安装器 + 定时任务 | done | | Claude Code / Codex 安装器 + 定时任务 |
| P1-11 | `init` / `doctor` / `uninstall` | done | | init / doctor / uninstall |
| P1-12 | 端到端测试 | done | | 逐字节往返 e2e |
| P1-13 | CLI 发布 | done | | 六平台二进制 + 安装脚本 + npm；发版改为 `just release` |
| P2-3 | 密钥区间扫描 | done | | `redaction::scan_spans` / `mask_text` |
| P2-0 | 本机采样与结构保持脱敏 | done | | 采样与匿名化工具 |
| P2-1 | `cormex-transcript` 与两个解析器 | done | | crate + 两个解析器（现共 15 个） |
| P2-2 | P 层 migration 与存储方法 | done | | P 层 migration 与存储 |
| P2-4 | 后台 worker：masker 与 projector | done | | masker / projector / indexer / distiller / promoter |
| P2-5 | 浏览与原文 API | done | | 浏览与原文 API |
| P2-7 | CLI `sessions ls / show` | done | | `sessions ls / show` |
| P2-6 | Web 会话视图 | done | | Web 会话视图；缺截图（见 verification-log） |
| P3-1 | rmcp 3.x + goose-providers 升级 | done | | rmcp 3.x + goose alpha.13 |
| P3-2 | 会话检索与原文档案索引 | done | | 会话检索与原文档案索引 |
| P3-3 | 会话的注入渲染（两个仓库） | done | | 注入渲染（drift 脚本已跑） |
| P3-4 | MCP 会话工具与资源 | done | | MCP 会话工具与简报资源 |
| P3-5 | 导出与 `cormex restore` | done | | 导出与 `cormex restore` |
| P3-6 | `cormex mcp` stdio 桥 | done | | stdio 桥 |
| P3-7 | MCP 注册、Agent Skill 与文档 | done | | MCP 注册 + SKILL.md |
| P4-0 | 快照批量接口 + SQLite 行来源 | done | | 快照批量 + SQLite 行来源 |
| P4-1 | Gemini CLI | done | | Gemini CLI |
| P4-2 | Cursor | done | | Cursor（从 CLI 打包 JS 读出格式） |
| P4-3 | VS Code Copilot Chat | done | | VS Code Copilot Chat |
| P4-4 | Cline / Roo Code | done | | Cline / Roo Code |
| P4-5 | OpenCode、Goose | done | | OpenCode、Goose |
| P4-6 | Copilot CLI、Qwen Code、Aider | done | | Copilot CLI（从二进制内嵌 JS + importer 读出）、Qwen、Aider |
| P4-7 | 导出文件进 R 层 | done | | 导出文件进 R 层 |
| P5-1 | 派生边扩展 | done | | `source_kind` 加 `transcript_event` |
| P5-2 | 观察日志 | done | | 观察日志 |
| P5-3 | 交接简报 | done | | 交接简报 |
| P5-4 | 按策略晋升与事实抽取 | done | | 晋升与事实抽取 |
| P5-5 | 事件时间轴 `at` | done | | 事件时间轴 `at` |
| P5-6 | 对话轮向量（默认关闭） | done | | 对话轮向量（默认关闭） |
| P6-1 | 每来源数据密钥 | done | | 每来源数据密钥 + AES-256-GCM |
| P6-2 | purge 覆盖 R 层 | done | | purge 覆盖 R 层 + redact 执行器 |
| P6-3 | 备份恢复演练 | done | | 数据库与对象存储两半都真跑过 |
| P6-4 | 设备码登录 + Web 设备页 | done | | 设备码登录 + Web 设备页 |
| P6-5 | npm 分发 | done | | npm 分发 |
| P7-1 | 无数据库的采集测试进 CI | done | | 无数据库的采集测试进 CI；CI 现为手动触发 |
| P7-2 | 会话检索评测集 | done | | 会话检索评测集 |
| P7-3 | LongMemEval-S 检索召回 | done | | LongMemEval-S 检索召回 |

## 偏离设计

（按 §12.4 记录：哪张卡、偏离了什么、证据、怎么做的、设计文档改了哪一段）

**没有偏离任何一条已拍板的决定（D1–D23）。** 三处实现细节与计划卡的措辞不同，
都是「卡里写的是手段，设计里写的是目的」，目的没变：

1. **CI 的 migration 顺序**（P0-1 / P7-1）。卡里写「CI 跑两套 migration」，
   实际必须按 `cortex_auth` → 迁移 → 改名回 `cormex_auth` 的顺序，因为 pinned
   的全局 migration 是全限定 `cortex_auth.…`。`ci_migration_order.rs` 钉住它。
2. **发版入口**（P1-13 / P6-5）。卡里写「打 tag 触发流水线」，现在 GitHub
   Actions 额度用光，改成 `just release <version>`；流水线保留为手动触发。
   两者做的是同一套事。
3. **`session_search` 空结果的形状**（P3-4）。卡里只要求「不满足权限时返回错误
   而不是空结果」；实现把「没有命中」也改成一句话，因为空 text 块与「工具坏了」
   无法区分 —— 真 CLI 就是这么误判的。

## 总结

55 张卡全部完成，代码在 `feat/redact-executor-final`，已合入 `main`。

**验证**（本地，`just ci-full`）：

```
cargo fmt --all -- --check                      无输出
cargo clippy --workspace --all-targets --locked -- -D warnings   通过
cargo test --workspace --locked -- --include-ignored --test-threads=4
                                                1088 passed, 0 failed
cd app && flutter analyze                        No issues found
cd app && flutter test                           33 passed
```

**不能自动化的验收项**见 [verification-log.md](verification-log.md)。

**这一轮修掉的、只有真跑才会暴露的 bug**（都已加回归测试）：

| 症状 | 原因 |
|---|---|
| 服务端在已有部署上起不来 | `migrate_global` 在跑 migrator 前就改名，而 pinned SQL 写死 `cortex_auth` |
| 所有 MCP 会话工具 30 秒超时 | `tenant()` 用 `futures::executor::block_on` 驱动 tokio future，等池连接时唤醒丢失 |
| `session_search` 返回空 text 块 | 空结果与「工具坏了」无法区分；真 CLI 据此判定工具失效 |
| CI 从 P1-1 起一直红 | CI 手工镜像了上面第一条的旧逻辑 |
| 备份演练在 CI 里必然失败 | `pg_dump` 16 对 pg17 服务端，版本不匹配 |
