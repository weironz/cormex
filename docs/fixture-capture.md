# 拿到各 agent 的真实会话文件

**全部解决了。** 这一页记录每条路径是怎么走通的，以及怎么自己复核。

---

## Cursor —— 从 CLI 自己的打包 JavaScript 里读出来的

公开文档没写，但 **CLI 是打包的 JavaScript**，序列化代码就在里面。

```bash
# 装 CLI（约 176 MB）
curl -fSL "https://downloads.cursor.com/lab/<version>/linux/x64/agent-cli-package.tar.gz" \
  | tar --strip-components=1 -xzf - -C /tmp/cursorcli

# 序列化代码：agent-transcript/dist/index.js 的 U（单条）与 W（整个文件）
grep -o 'function U([^)]*)[^}]*}' /tmp/cursorcli/7923.index.js | head -1
```

格式：

```text
{"role":"user","message":{"content":[{"type":"text","text":"…"}]}}
{"role":"assistant","message":{"content":[
    {"type":"text","text":"…"},
    {"type":"tool_use","name":"bash","input":{…}}]}}
{"type":"turn_ended","status":"success"}
{"type":"metadata","metadata":{"overview":"…"}}
```

路径：`~/.cursor/projects/<workspace 路径压成 - >/agent-transcripts/<id>.jsonl`
（`utils/dist/workspace-paths.js` 里 `O2 = "agent-transcripts"`）。

### 三个容易写错的点（解析器已处理）

1. **`thinking` 被合并进 `text` 块** —— `U` 把消息的 `text` 与 `thinking` 拼成
   一个块，事后分不开。解析器不假装能分。
2. **`tool_result` 被丢掉** —— `R` 只记 `{toolName}`，`U` 根本不输出它。所以
   **工具输出不在这份文件里**，它在 blob store 里。
3. **连续两条 user 消息的第一条被丢掉** —— `W` 干的，因为 Cursor 把 system
   prompt 写成一条 `user` 消息。

---

## Copilot CLI —— 两条路都走通了

### 路径 A：二进制里打包着应用 JS

CLI 是 stripped 原生二进制（198 MB），但它内嵌了一份 **gzip 的 tar**，里面是
应用代码（`package/cli-main.js`，8.3 MB）：

```bash
python3 - <<'PY'
import io, tarfile, zlib
data = open('node_modules/@github/copilot-linux-x64/copilot','rb').read()
off = data.find(b'\x1f\x8b\x08', 100_000_000)   # 内嵌 tar 的 gzip 头
raw = zlib.decompressobj(16 + zlib.MAX_WBITS)
buf = io.BytesIO(); pos = off
while pos < len(data) and not raw.eof:
    buf.write(raw.decompress(data[pos:pos+(1<<20)])); pos += 1<<20
buf.seek(0)
tarfile.open(fileobj=buf).extract('package/cli-main.js', '/tmp')
PY

grep -oE '"(session|user|assistant|tool)\.[a-z._]+"' /tmp/cli-main.js | sort -u
```

事件类型、`toolRequests` 的结构、`tool.execution_complete` 的字段，都是从那里
读出来的。

### 路径 B：`copilot sessions import` 会写出真实的 `events.jsonl`

这条路**不需要认证** —— import 只写本地文件。而且它的校验错误会**逐字段**告诉
你 schema：

```bash
echo '{}' > t.jsonl
copilot sessions import t.jsonl --dry-run
# Line 1: The first record must be a session header. [missing_session_header]

echo '{"type":"session"}' > t.jsonl
copilot sessions import t.jsonl --dry-run
# Line 1: Session interchange version must be 1. [unsupported_version]
# Line 1: Session externalId must be a non-empty string. [invalid_external_id]
```

一路问下去就得到完整 schema：

```jsonl
{"type":"session","version":1,"externalId":"x","createdAt":"2026-10-30T00:00:00Z"}
{"type":"message","id":"m1","timestamp":"2026-10-30T00:00:01Z","role":"user","content":[{"type":"text","text":"hello"}]}
{"type":"message","id":"m2","timestamp":"2026-10-30T00:00:02Z","role":"assistant","content":[{"type":"text","text":"hi there"}]}
```

导入之后，`~/.copilot/session-state/<id>/events.jsonl` 就是**真实存储格式**。
本仓库的夹具 `crates/cormex-transcript/tests/fixtures/copilot_cli_session.jsonl`
就是它，逐字复制。

### 真实格式

```text
{"type":"session.start","data":{"sessionId":"…","startTime":"…","context":{"cwd":"…"}},"id":"…","timestamp":"…","parentId":null}
{"type":"user.message","data":{"content":"hello"},"id":"…","timestamp":"…","parentId":"…"}
{"type":"assistant.message","data":{"messageId":"…","content":"hi there"},"id":"…","timestamp":"…","parentId":"…"}
```

工具调用**不在单独的事件里** —— 它在 `assistant.message` 的
`data.toolRequests[]` 里（`{toolCallId, name, arguments}`）；结果在
`tool.execution_complete`（`{toolCallId, success, result.content}`）。

---

## 复核

```bash
cargo test -p cormex-transcript
```

`copilot_cli::tests::it_reads_a_real_events_jsonl` 读的是真实文件，不是按 schema
手写的 —— 那是唯一能证明「schema 读对了、字段名也用对了」的测试。