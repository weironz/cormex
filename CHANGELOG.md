# Changelog

## Unreleased

### Capture: the same conversation is archived once, whatever happens

- A file the server has never seen under this device and path is offered the generations other files of the **same session and role** already hold. The client proves its bytes match one with a prefix hash (`POST /capture/generations/{id}/verify`) and follows it (`POST /capture/files/{id}/adopt`) instead of uploading the history again. Covered: reinstalling with a **new** device key, Codex moving a rollout into `archived_sessions/`, a renamed Claude Code project directory, a `.orphaned-…` set-aside copy, the same session on a second machine. Before, each of these uploaded every byte again and the projector showed the conversation twice.
- The client no longer resumes on an offset alone. After `state.db` is lost, and whenever the server answers 409 "I am ahead", it verifies the server's prefix against its own bytes first: identical → continue at the server's end; local is an older copy → send nothing; different bytes (the file was rewritten while nobody was watching) → a new generation, with the old one left intact. The old behaviour spliced a rewritten file onto the previous history without a word.
- Two machines appending to one session now append to one generation; the second machine's identical bytes are verified and skipped.
- New migration `20261127000001_capture_generation_adoptions.sql` (table `capture_generation_adoptions`, view `capture_current_generations`). Append-only; `ON DELETE CASCADE` so purge is unaffected.
- Not covered: conversations that were already duplicated before this change stay duplicated in the archive until a repair pass is run.

### Archive browser: expanding a project no longer scans the whole archive

- `GET /archive/sessions?project=…` grouped every event of every session and only then kept the project's sessions, so each expansion cost a sequential scan of `transcript_events` that grew with the whole archive. The project / agent / unassigned scope is now resolved against `capture_sessions` first, and only the surviving sessions are aggregated, each through its own index range. On a 1.2 M-event fixture (3 000 sessions, 60 projects): expanding a project 300 ms → 14 ms; a searched expansion 1.7 s → 12 ms.
- New migration `20261128000001_transcript_events_user_text_index.sql`: a partial index on `(session_id, seq)` for non-empty user messages, which is where every untitled row's title comes from. Without it each title read the session's events and sorted them, and the inflated cost estimate pushed the query over PostgreSQL's JIT threshold — the compile alone was 0.7–1.3 s of the searched case. `CREATE INDEX` takes a write lock on `transcript_events` while it builds; on the fixture it took 0.3 s.
- A page's "only copy" flags are one query instead of one per row.
- A session whose files carry different project keys — a renamed project directory leaves the old file behind — is listed under its **newest** file's project. It used to take the alphabetically smallest key, so a renamed project could keep showing its chats under the old name. The facet counts, the expanded list, the detail view and the indexer's exclusion check all use the same rule.

### Archive browser: layout and reading

- On wide screens the two pages move to a side rail instead of a bottom bar, so the archive reads left to right: where to go, which chat, the chat itself. Phones keep the bottom bar.
- The project list is a directory rather than a stack of cards: projects are group rows with a count, chats are rows under them with a two-line title (the first question's topic is usually in its second half), a source dot, the message count and the date. Sources show their product names everywhere (`Claude Code`, not `claude_code`), from one table in `core/agents.dart`.
- The source filter is a small chip next to the totals, not a full-width dropdown.
- The detail pane's header names the project; the chat's title, source, date range and length sit at the top of the conversation, where a long title can wrap instead of being cut off. In the split view the two headers sit side by side at the same height.
- Questions get a tinted block and answers have no frame, so each turn's start is easy to find. Code blocks get a bordered panel and a monospace face.
- Dates stay absolute; a chat from this year drops the year (`11月7日`).

### Capture CLI

- `cormex login --print-key` prints the stored device key — the only copy of this machine's identity, since the server keeps its sha256 and cannot re-issue it. The key goes to stdout alone, so `cormex login --print-key > key.txt` is a working backup; the "where it came from" line goes to stderr. Restore after a reinstall with `cormex login --key`, and the server sees the same device and merges rather than capturing every file again under a new `device_id` — which duplicates the projected events, because the event id derives from the generation.
- `cormex uninstall` now says where the device key survives (keyring entry, credentials file, or `CORMEX_DEVICE_KEY`). Uninstall removes the hooks, not the login — a reinstall that reuses the key is the cheap path.

### Deployment

- `deploy/docker-compose.yml` now states `archive_mode=off` and `max_wal_size=2GB` explicitly, matching the other two compose files. It had no `command` at all, which worked only because PostgreSQL defaults archiving to off; there was no bound on WAL growth either. The header comment used to say a failing archive is "treated as finished" and pointed to the prod file as a PITR reference — both wrong now, so it says what actually happens and what PITR would require.
- `deploy/docker-compose.prod.yml` no longer enables WAL archiving. It had `archive_mode=on` with an `archive_command` writing to `/backup/wal`, but `/backup` is a root-owned bind mount while the archiver runs as `postgres` (uid 999), so every attempt failed and PostgreSQL could never recycle a WAL segment. Measured on node72: 49 GB of `pg_wal` / 3079 `.ready` files for a 3.4 GB database. Archived WAL was never used — backups are `pg_dump` plus the object-store volume — so archiving is off, `archive_timeout=60` is gone, and `max_wal_size=2GB` now bounds the WAL. Root `docker-compose.yml` had already made this change; the prod reference had not.

## 0.1.15 — 2026-11-03

### Release tooling

- `just release` now publishes the CLI archives to the GitHub Release (`release-assets`). It used to build `dist/`, push the images and stop — so a local release produced new images and updated no CLI on any machine, because the archives are the only channel `cormex update` and `install.sh` read. It refuses to publish when the tag is not on the origin or does not point at `HEAD`: `gh release create` given an unknown tag creates it at the default branch's head, which would attach archives built from a working tree to a commit nobody built.
- The local path now works on Windows. Host detection used a glob inside test brackets (`[ "$host" = MINGW64_NT-* ]`), which compares against the literal string and matches nothing, so both Windows targets were skipped *on Windows*. `zip` — absent from Git for Windows — falls back to Python's `zipfile`. The `--version` smoke test runs the host's own binary instead of the musl one, which cannot execute there.
- The npm channel is explicitly off, gated on `PUBLISH_NPM=true` **and** `NPM_TOKEN`. `npm install -g cormex` had been listed as an install option while the package has never been on the registry.
- `CLAUDE.md` is now `AGENTS.md`, and the release section documents both paths — the workflow (six platforms, natively) and `just release` (only what the host can target).

## 0.1.14 — 2026-11-03

### Capture CLI

- `cormex update` self-updates the binary: it fetches the release archive for this platform, checks it against the release's `SHA256SUMS`, unpacks it, runs the new binary once, and only then replaces the current file in place. A failed download or a bad checksum leaves the installed binary untouched. `--check` reports without downloading, `--version` pins a release, `--force` reinstalls or downgrades.
- The replacement happens at the same path, so the absolute paths `init` recorded in hooks and in the scheduler keep working; nothing has to be re-run.
- Binaries installed by npm and `target/debug` development builds are refused unless `--force` is given.

## 0.1.13 — 2026-11-01

### Capture and authentication

- Support both legacy and current OpenCode SQLite layouts. One agent's scan failure no longer aborts initialization for the others.
- Preserve Codex projects, original chat titles, and the origin of archived chats. Empty projects are omitted.
- Improve device authorization and persist Windows credentials across processes. Reuse valid authentication instead of requiring repeated login.
- Automate initialization and initial history capture.

### Chat archive

- Load project directories and chats through server-side pagination, with lazy loading when projects are expanded.
- Separate human inputs, final replies, and internal context/process records. Render Markdown and group preparation/process details.
- Replace overflowing agent tabs with a source selector, remove the pinned category, and add persistent reading-width settings.

### Automatic memory

- Keep two views: memories and conversation summaries. No human approval queue is required.
- Extract from complete QA turns, bounded context, and archived tool evidence. Automatically validate meaning, retain useful information, deduplicate it, and append updated versions.
- Preserve source evidence and explain applicability. Keep plans distinct from implemented capabilities, and avoid promoting unadopted advice or one-off task assignments to facts.
- Cache extraction separately from semantic validation, so validator retries do not repeat the first model call.
- Keep historical full extraction opt-in. Small-sample trials remain bounded and do not enable background extraction.

### Upgrade

- Includes additive migrations for project metadata, pagination counters, extraction settings, trial progress, and validation caches.
- Back up the database before upgrading. Existing source records and fact history are retained; invalidation is recorded through append-only events.
- Model credentials are encrypted using a persistent, deployment-owned master key. Do not copy development credentials or chat data into production.
