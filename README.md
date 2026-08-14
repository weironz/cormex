# Cormex

**Memory that remembers what you used to believe.**

A memory server for AI agents. Bitemporal, provenance-tracked, MCP-native.

> ⚠️ Early. Extracted from a working product, but the packaging is not done yet —
> no releases, no stable API guarantees. See [Status](#status).

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
cortex-core     types / ids / config / errors
cortex-llm      provider layer (wraps goose-providers)
cortex-store    sqlx repositories + the sync log
cortex-memory   tokenization / embeddings / extraction / 4-way recall + RRF
cortex-blob     content-addressed blob storage (S3 / local)
cortex-proto    the wire protocol
cortex-import   importers for ChatGPT / Claude conversation exports
cortexd         the server: HTTP + SSE + WebSocket + MCP
```

Postgres with pgvector. That is the only hard dependency.

## Status

This code runs in production as the memory layer of a private agent product.
It was extracted into its own repository so it can be used on its own.

**Not done yet:**

- The Flutter memory browser (search, provenance, the draggable timeline) still
  lives in the parent repo and has to be carved out.
- No published releases, no Docker image, no `cargo install` path.
- Comments and docs are in Chinese. They are unusually thorough — they explain
  *why not the obvious thing* at every non-obvious decision — and translating
  them without losing that is a real task, not a `sed`.
- API is not frozen.

## License

Apache-2.0. See [`NOTICE`](NOTICE) for vendored components — the provider layer
and the sandbox primitives are lifted from [goose](https://github.com/block/goose)
and [codex](https://github.com/openai/codex), both Apache-2.0.
