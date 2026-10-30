# 会话档案 —— 验收记录

> 规则见 [session-archive-plan.md](session-archive-plan.md) §12.6。
> 任务卡里不能自动化的验收项（「手动验证」「PR 里贴步骤」），逐条记在这里。

每条的格式：

```text
### <卡号> <验收项>
- 环境：<在哪里做的>
- 步骤：<命令或操作>
- 结果：通过 / 失败（附关键输出）/ 待所有者验证
- 所有者验证步骤（仅「待所有者验证」时）：<所有者照着做就能验证的步骤>
```

## 记录

### P3-1 / P3-4 真 `claude` CLI 连 `/mcp` 能调工具

- 环境：开发机，cormexd 以宿主进程跑在 `127.0.0.1:18080`，Postgres 在容器里
- 步骤：
  ```bash
  cat > /tmp/mcp-probe.json <<'EOF'
  {"mcpServers":{"cormex":{"type":"http","url":"http://127.0.0.1:18080/mcp",
   "headers":{"Authorization":"Bearer <token>"}}}}
  EOF
  claude -p "call memory_search, session_search, read cormex://profile" \
    --mcp-config /tmp/mcp-probe.json --strict-mcp-config \
    --allowedTools "mcp__cormex__memory_search,mcp__cormex__session_search"
  ```
- 结果：**通过**。三个调用都返回正常，无超时、无报错：
  ```text
  memory_search 'rustfs'  → 没有检索到相关记忆。
  session_search 'rustfs' → 没有检索到相关会话。
  cormex://profile        → {"contents":[{"uri":"cormex://profile",
                             "mimeType":"text/plain","text":""}]}
  ```
  结果为空是因为那个库确实是空的（`capture_sessions` / `transcript_events` /
  `episodes` 三张表都是 0 行），不是工具失效。
- 备注：这一轮正是从这里抓出了「所有 MCP 会话工具 30 秒超时」和
  「`session_search` 返回空 text 块」两个 bug，两者都已修并加了回归测试。

### P6-3 备份恢复演练（对象存储那一半）

- 环境：开发机，`just up` 起的 RustFS 容器
- 步骤：`scripts/rustfs-volume-drill.sh`
- 结果：**通过**。写对象 → `tar` 卷 → 解进新卷 → 在新卷上起第二个 RustFS →
  从恢复后的 store 读回 → 逐字节一致。
  ```text
  == 1. write an object through the S3 API ==
  == 2. back up the volume ==
  == 3. restore into a new volume ==
  == 4. start a second RustFS on the restored volume ==
  == 5. read the object back from the restored store ==
     byte-for-byte match
  drill passed
  ```

### P6-3 备份恢复演练（数据库那一半）

- 环境：开发机，Postgres 17 容器
- 步骤：`cargo test -p cormex-store --test backup_restore_drill -- --include-ignored`
- 结果：**通过**。建库 → 迁移 → 塞入已擦除的 episode 与墓碑 → `pg_dump` →
  恢复进空库 → 逐表比对 → 断言擦除内容没回来。

### P1-11 `init` → `uninstall` 在干净容器里还原字节

- 环境：`debian:bookworm-slim` 容器，musl 静态二进制
- 步骤：容器内跑 `cormex init`（写入 hook 配置）→ `cormex uninstall`
- 结果：**通过**，`RESTORED BYTE FOR BYTE`。

### P1-13 发版

- 状态：**待所有者执行**。
- 原因：发版要打 tag 并推两个 registry，是对外动作。
- 所有者验证步骤：
  ```bash
  just release-check 0.1.12   # 版本与 Cargo.lock 自洽
  just release 0.1.12         # 构建 → 烟测 → 推送 → npm
  ```
  `just release` 在推送前完成全部构建与烟测；`NPM_TOKEN` 未设置时 npm 那步会跳过
  并说明。

### P2-6 Web 会话视图

- 环境：开发机，`flutter build web` 的产物由一个小代理伺服（`/api` 与 `/health` 转发到
  cormexd，其余走静态文件），Playwright 的 headless Chromium 驱动
- 步骤：造一个 Claude Code 会话文件 → `cormex detect` → `cormex sync`（真设备密钥）
  → 等 projector 投影出 4 条事件 → 浏览器登录 → 进「会话档案」
- 结果：**通过**。

  列表页（`docs/images/p2-6-sessions-list.png`）：标题取自第一条用户消息
  「我们的对象存储用的什么？」，下面一行是 `claude_code · 4 条 · 2026-10-28`。

  详情页（`docs/images/p2-6-session-detail.png`）：四条事件按序排列，每条带角色
  （用户 / agent）和**绝对日期**（2026年10月28日 17:00），正文完整。

  两张图都是真数据、真服务端、真浏览器渲染，不是设计稿。

### P3-7 `npx skills add weironz/cormex`

- 状态：**待所有者验证**（需要已发布的仓库可见性）。
- 所有者验证步骤：`npx skills add weironz/cormex`，确认 `skills/cormex/SKILL.md`
  被装上。
