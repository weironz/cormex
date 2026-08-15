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
cortex-core     types / ids / config / errors
cortex-llm      provider layer (wraps goose-providers)
cortex-store    sqlx repositories + the sync log
cortex-memory   tokenization / embeddings / extraction / 4-way recall + RRF
cortex-blob     content-addressed blob storage (S3 / local)
cortex-proto    the wire protocol
cortex-import   importers for ChatGPT / Claude conversation exports
cortexd         the server: HTTP + SSE + WebSocket + MCP
app/            the memory browser (Flutter — web and desktop from one source)
```

Postgres with pgvector. That is the only hard dependency.

## Running it

Two images: `cormex` (the server) and `cormex-web` (the browser). They are
separate containers because they are separate concerns — you can run the server
alone and talk to it over MCP, and never deploy the browser at all.

```bash
cd deploy
cp .env.example .env      # set CORMEX_DOMAIN, CORMEX_VERSION, the token hash
docker compose up -d
```

> ⚠️ **That compose file is our production one and will not start on a fresh
> machine** — its volumes are declared `external` under names our deployment
> already has, so you get `external volume "cortex_cortex-prod-postgres" not
> found`. A self-contained single-host compose is the top item on the
> [roadmap](docs/roadmap.md); until it lands, take that file as a reference for
> the shape rather than something to run.

**The web image carries no domain name.** The API address compiled into it is
the relative prefix `/api`, which the browser resolves against whatever origin
served the page. So one published image works for every deployment; you route
`/api` to the server and everything else to the browser, at your edge proxy.
The compose file does that with traefik labels, and they are the only place a
hostname appears.

That relative-prefix detail is the difference between an image anyone can pull
and an image whose first job is to be rebuilt.

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
