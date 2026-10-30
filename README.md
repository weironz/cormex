# Cormex

**Memory that remembers what you used to believe.**

A memory server for AI agents. Bitemporal, provenance-tracked, MCP-native.

> ⚠️ Early. Extracted from a working product. Docker images are published; the
> API is not frozen. See [Status](#status).

---

## The difference

Most agent-memory projects give you hybrid retrieval, a knowledge graph, and
conflict detection. Cormex has those. They are not the point.

The point is that **nothing is ever erased, and you can replay what the agent
knew at any past instant.**

```
"What do we use for object storage?"        → RustFS.
"What did I think three months ago?"        → MinIO.
"Why do you believe the current answer?"    → [this conversation, 2026-06-14]
```

Three things make that possible, and no other memory layer does all three:

**Two time axes.** Every fact carries both *valid time* (when it was true in the
world) and *transaction time* (when this system learned it). Queries take an
`as_of` — you get the memory as it stood then, not a filtered view of today's.

**Append-only, always.** Facts are never updated or deleted. Invalidation is a
new row in `fact_events`. So "what changed and when" is not an audit log bolted
on the side — it *is* the storage model.

**Provenance to the source.** Every fact points back at the episode it was
extracted from, with a trust tier attached. A fact that came from a tool's
output is not the same kind of thing as one you said out loud, and the system
knows the difference.

## MCP-native

Not "has an MCP adapter". The two MCP primitives map onto the two halves of
context injection, and that is why the design works:

| | MCP primitive | why |
|---|---|---|
| Core profile block | **resource** (`cormex://profile`) | The host pins it into the cacheable prefix. It does not change per turn, so it does not break prompt caching. |
| Per-turn retrieval | **tool** (`memory_search`) | The model calls it when it needs to; the result naturally lands next to the latest message. |

A naive memory plugin concatenates retrieval results into the system prompt
every turn. That invalidates the prefix cache on every single turn — 5–10× the
cost, and you only find out weeks later from the bill.

Every block Cormex returns is framed as **data, not instructions**. Memory can
contain text that was extracted from an untrusted source; the framing is the
first fence against memory poisoning, and it is applied on the tool path and the
injection path by the same function.

## Delegated credentials

Cormex can mint a **scoped, short-lived key** for an untrusted caller — bound to
one session, with an explicit allowlist of what it may call. You can hand an
agent sandbox read/write access to one slice of memory without handing over the
user's credentials.

The caller mints with *its own* bearer, so it can only ever act for the user who
gave it that token. No service key, no privilege escalation surface.

## What's in the box

```
cormex-core     types / ids / config / errors
cormex-llm      provider layer (wraps goose-providers)
cormex-store    sqlx repositories + the sync log
cormex-memory   tokenization / embeddings / extraction / 4-way recall + RRF
cormex-blob     content-addressed blob storage (S3 / local)
cormex-proto    the wire protocol
cormex-import   importers for ChatGPT / Claude conversation exports
cormexd         the server: HTTP + SSE + WebSocket + MCP
app/            the memory browser (Flutter — web and desktop from one source)
```

Postgres with pgvector. That is the only hard dependency.

## Running it

Two images: `cormex` (the server) and `cormex-web` (the browser). They are
separate containers because they are separate concerns — you can run the server
alone and talk to it over MCP, and never deploy the browser at all.

```bash
docker run --rm willdockerhub/cormex:v0.1.11 --generate-token

cd deploy
cp .env.example .env      # paste the hash it printed, set two passwords
docker compose up -d
```

Then open <http://127.0.0.1:8080>. Everything binds to the loopback, so the
host is the boundary — put a proxy with TLS in front before exposing any of it.

`docker-compose.prod.yml` beside it is our own deployment, and shows the shape
a real one takes: an edge proxy splitting `/api` from the browser, WAL
archiving to a separate disk, volumes that already exist under names nobody can
change. Read it; do not run it.

**The web image carries no domain name.** The API address compiled into it is
the relative prefix `/api`, which the browser resolves against whatever origin
served the page. So one published image works for every deployment; you route
`/api` to the server and everything else to the browser, at your edge proxy.
The compose file does that with traefik labels, and they are the only place a
hostname appears.

That relative-prefix detail is the difference between an image anyone can pull
and an image whose first job is to be rebuilt.

## Capture your agent sessions

A small CLI reads the session files your AI agents already write to disk and
sends them, byte for byte, to your Cormex server. It never modifies them and
never parses what it reads — parsing happens on the server, so twenty hosts'
worth of format churn is maintained in one place rather than in every installed
binary.

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/weironz/cormex/main/scripts/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/weironz/cormex/main/scripts/install.ps1 | iex

# or, if you already have Node
npm install -g cormex
```

Then:

```bash
cormex login --server https://memory.example.com   # asks for your account password
cormex init                                        # hooks, scheduler, backfill
```

On a machine with no browser (a server, a container), use the device-code flow
instead — it prints a short code, you type it into a browser you are already
logged into, and the machine polls until a key appears:

```bash
cormex login --server https://memory.example.com --device-code
```

`cormex init` shows every change before making it: hooks in the agent's own
settings file, a periodic sync, and a raised `cleanupPeriodDays` (the host
deletes old sessions, and a capture client cannot recover what is already gone).
`cormex uninstall` restores each file from a backup — byte for byte, not
"removes what it thinks it added".

### What it reads, and what it does not

| | |
|---|---|
| **Reads** | the agent's own directories: `~/.claude/projects`, `~/.codex/sessions`, and so on |
| **Does not read** | your project directories, your source code, anything outside those homes |
| **The one exception** | `git remote get-url origin` and `git rev-parse HEAD` in a session's working directory, for the project filter. Turn it off with `git_metadata = false` |

The device key is scoped: `capture:write` by default, plus `session:read` and
`memory:read`. `raw:read` — the original, unmasked bytes — has to be asked for
explicitly, so a lost laptop does not hand over every conversation.

### Connect an agent over MCP

`cormex init` registers Cormex with the agents it finds. To do it by hand:

**Claude Code** (streamable HTTP):

```bash
claude mcp add --transport http cormex https://memory.example.com/mcp \
  --header "Authorization: Bearer <device key>"
```

**Codex** (stdio — Codex spawns a child process, and `cormex mcp` forwards to
the server):

```toml
# ~/.codex/config.toml
[mcp_servers.cormex]
command = "/usr/local/bin/cormex"
args = ["mcp"]
```

Either way the agent gets `memory_search`, `remember`, `session_search` and
`session_get`, plus the `cortex://profile` resource. The session tools are the
reason to connect: they let an agent answer "what did we decide last week, and
why" from the actual conversation rather than from a summary of it.

`cormex doctor` answers "why is this machine not capturing": server
reachability, credential storage, queue backlog, last successful run, whether
the hooks are still in place.

## Status

This code runs in production as the memory layer of a private agent product.
It was extracted into its own repository so it can be used on its own.

**Not done yet:**

- No `cargo install` path, and no published crates. Docker is the only packaging.
- Comments in `app/` and `deploy/` are still Chinese. The Rust crates have been
  translated; those two have not. They are unusually thorough — they explain
  *why not the obvious thing* at every non-obvious decision — so translating
  them without losing that is a real task, not a `sed`.
- API is not frozen.

## License

Apache-2.0. See [`NOTICE`](NOTICE) for vendored components — the provider layer
and the sandbox primitives are lifted from [goose](https://github.com/block/goose)
and [codex](https://github.com/openai/codex), both Apache-2.0.
