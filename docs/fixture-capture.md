# 拿到 Copilot CLI 的真实会话文件

**Cursor 已经解决了**（见下）；只剩 Copilot CLI。

Copilot CLI 的格式拿不到：`github/copilot-cli` 是**闭源仓库**（只有 README /
changelog / install.sh），CLI 分发成**原生二进制**（`@github/copilot-linux-x64`，
198 MB，stripped），字符串里没有 transcript schema。装上去跑对话也试过 ——
它拒绝启动，除非有 **fine-grained** GitHub PAT 或 OAuth（`ghp_` 开头的 classic
token 被点名拒绝），所以没能产出 `events.jsonl` 来看。

---

---

## 为什么现在采不到也能接受

`crates/cormex-transcript/src/copilot_cli.rs` 现在做的是：**采字节，投影如实标注**。

- 字节进 R 层（对象存储 + `capture_chunks`），原样保留
- 投影是一条 `Unknown` 事件，`raw_type = "copilot_cli:format-undocumented"`
- 解析器版本是 `copilot_cli@1-undocumented`

这意味着：**今天采到的文件，明天写解析器之后能重算**。反过来（不采）就不行 ——
宿主会删自己的历史，没采到的文件永远补不回来。

---

## Cursor —— 已经解决了

**不需要样本了。** Cursor CLI 是打包的 JavaScript，序列化代码就在里面：

```
agent-transcript/dist/index.js   → 函数 U（单条消息）与 W（整个文件）
utils/dist/workspace-paths.js    → O2 = "agent-transcripts"，路径压缩规则
```

解析器照着那段代码写的（`crates/cormex-transcript/src/cursor.rs`，版本
`cursor@2`）。格式是：

```text
{"role":"user","message":{"content":[{"type":"text","text":"…"}]}}
{"role":"assistant","message":{"content":[
    {"type":"text","text":"…"},
    {"type":"tool_use","name":"bash","input":{…}}]}}
{"type":"turn_ended","status":"success"}          ← 仅在一轮结束时
{"type":"metadata","metadata":{"overview":"…"}}   ← 仅有 overview 时
```

文件在 `~/.cursor/projects/<workspace 路径压成 - >/agent-transcripts/<id>.jsonl`。

### 怎么自己确认

```bash
# 装 CLI（约 176 MB）
curl -fSL "https://downloads.cursor.com/lab/<version>/linux/x64/agent-cli-package.tar.gz" \
  | tar --strip-components=1 -xzf - -C /tmp/cursorcli

# 序列化代码
grep -o 'function U([^)]*)[^}]*}' /tmp/cursorcli/7923.index.js | head -1
```

### 三个容易写错的点（解析器已经处理）

1. **`thinking` 被合并进 `text` 块** —— `U` 把消息的 `text` 与 `thinking` 拼成
   一个块，事后分不开。解析器不假装能分。
2. **`tool_result` 被丢掉** —— `R` 只记 `{toolName}`，`U` 根本不输出它。所以
   **工具输出不在这份文件里**，它在 blob store 里。
3. **连续两条 user 消息的第一条被丢掉** —— `W` 干的，因为 Cursor 把 system
   prompt 写成一条 `user` 消息。

---

## Copilot CLI

### 0. 先要有 fine-grained PAT

本机 `gh` 用的是 classic PAT，Copilot CLI 明确拒绝。需要：

- 一个 **fine-grained** personal access token（GitHub → Settings → Developer
  settings → Personal access tokens → Fine-grained tokens），或
- `copilot` 交互模式里 `/login` 走 OAuth

```bash
export COPILOT_GITHUB_TOKEN=<fine-grained-pat>
```

### 1. 找到会话目录

设计给的路径是 `~/.copilot/session-state/*/events.jsonl`。先确认它真的在那里：

```bash
ls -la ~/.copilot/session-state/ 2>/dev/null | head
find ~/.copilot -name "events.jsonl" 2>/dev/null | head
```

**如果路径不对**，那就是设计里的假设需要修正 —— 把实际路径记下来，改
`crates/cormex-proto/catalog/agents.toml` 里 `copilot_cli` 的 `glob`。

### 2. 跑一段对话

```bash
copilot -p "hello"    # 或者进交互模式随便聊两句
```

### 3. 采一份样本

```bash
cp "$(find ~/.copilot -name events.jsonl | head -1)" \
   crates/cormex-transcript/tests/fixtures/copilot_cli_session.jsonl
```

同样先脱敏。

### 4. 写解析器

`cursor.rs` 里 `Undocumented::CopilotCli` 那条分支，版本改成 `copilot_cli@2`。

---

## 如果 Copilot CLI 也拿不到

那就保持现状：字节在采，投影标注「格式未知」。这不是欠账 —— 它比「采了但假装
能解析」好，因为编出来的对话比缺失更难发现。

---

## 拿到之后要改的地方

| 文件 | 改什么 |
|---|---|
| `crates/cormex-transcript/src/copilot_cli.rs` | 真的解析；版本 `@2` |
| `crates/cormex-transcript/tests/fixtures/` | 放脱敏后的样本 |
| `crates/cormex-proto/catalog/agents.toml` | 如果实际路径与设计不符，改 `glob` |

改完跑：

```bash
cargo test -p cormex-transcript
cargo test --workspace --locked -- --include-ignored
```

投影器会在下一轮把已存字节重算成事件 —— 不需要重新上传任何东西。