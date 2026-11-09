# Changelog

## Unreleased

## 0.1.21 — 2026-11-09

### Memories per session, and finding things by id

- **Generate memories for one conversation.** Each archived session has a "生成记忆" button: it runs the automatic worker's own per-session pass on that conversation, at most 30 QA turns per click, resuming where the last click stopped. It asks first and states how many turns it will send. `POST /archive/sessions/{id}/memory` starts it, `GET` reports pending turns, memory count and the job; it shares the one explicit-job slot with small-sample runs and works whether automatic extraction is on or off.
- A session-scoped advisory lock (4274) keeps the automatic worker and a per-session job from analysing the same session at once. Already-analysed turns are never sent again.
- **Session ↔ memory mapping.** `GET /archive/sessions/{id}/memories` lists the memories a conversation produced (superseded ones marked, not hidden); the session page shows its count and opens the list, and a memory's detail shows its own id and its session's id.
- **Look up any id.** `GET /lookup/{id}` says whether an id is a session, memory, summary or source episode; "按 ID 定位" in the memory and archive pages opens it. Memory cards and rows show a short id that copies the full one.
- The memory page switches between cards and a compact list; the choice is remembered.

### Release tooling

- `build-local` starts the selected buildx builder before recording the build runtime identity. A stopped builder reports no BuildKit version until the first image build starts it, which made the end-of-build check refuse the first v0.1.20 candidate with "runtime changed during local build".
- `deploy-upgrade` runs on Python 3.10 deployment hosts again: the host-side script no longer uses `hashlib.file_digest` (3.11+), which stopped the first v0.1.20 upgrade right after its backup and before any configuration change.
- Ask the registry again when a tag lookup is ambiguous before failing closed. Docker Hub intermittently answers a lookup for a missing tag with 401 instead of 404, which stopped the v0.1.20 publication twice before any push.

## 0.1.20 — 2026-11-09

### Operations: validated local builds and explicit deployment upgrades

- Add `just doctor`, `just doctor-deploy` and `just prepare-version` to check the selected runtime and prepare consistent release metadata before building. `just build-resume` revalidates reusable artifacts and always reruns the complete CI gate; build and publish operations write stage receipts outside the immutable candidate.
- Preserve Cargo download and compilation caches across release version changes. Verify approved image content across Docker image stores instead of comparing machine-specific image IDs, and record published registry digests.
- Add `just deploy-upgrade FROM TAG BUNDLE_SHA256`: back up the database and configuration, verify approved image content, pin registry digests and run shared server/Web checks. Receipts distinguish estimated row counts, readable backups and restore rehearsals; configuration is restored automatically only before a start attempt.

### Memory: a same-day update dated only "today" supersedes again

- When the extractor wrote an update's event time as a bare date ("now" becomes `2026-11-08`, which parses to midnight), contradiction resolution ordered the update *before* the statement it replaced. That statement had been made earlier the same day and ordered by the moment it was said. The update was filed as backfilled history, superseded nothing, and both values stayed current, so replay showed stale answers as present. A date-only event time within a day of when it was said now orders at the moment it was said. A date further back is still a real event time, so backfills keep their place. Found while recording the README's `as_of` walkthrough.
- README: a real `as_of` run over MCP (MinIO, then RustFS a few seconds later; asked now and as of the moment between), with what it depends on: superseding needs the extractor to name both facts with the same predicate.

### Memory: an English adoption is an adoption; extraction gets an eval

- Automatic extraction discarded every decision a user adopted in English. The rule that keeps an AI suggestion only once the user adopts it read adoption by Chinese words alone, so "Agreed, we adopt absolute dates" counted as an unadopted suggestion. It now reads both languages, and "disagree" is checked as a refusal before "agree" can count. The rule version (`qa@4`) is unchanged, so nothing is re-extracted or re-billed; the fix applies to new analyses.
- `cormex-evals extract` scores the extractor production actually uses (the dialogue analyser, `qa@4`). The existing `run --mode llm` measured the older per-episode extractor. The new suite, `extraction-zh-en-v1`, has 24 cases in 13 categories, with what must be retained and what must never be (credentials, declined suggestions, injected instructions, small talk). `just eval-check` runs `validate --extraction`. Baseline on `deepseek-flash`: 83% recall and a 7% leak rate on the first run, which found the bug above; 100% and 0% after the fix. That means the suite is saturated, not that extraction is solved (see `evals/README.md`).

### Memory: turn full extraction on for a recent stretch first

- Full extraction has a scope: the last 7, 30 or 90 days, or every chat (Settings → 模型与记忆提取 → 提取范围). Before, it was one switch that meant the whole archive (4510 sessions on our production tenant, plus thousands in one vendor export), so nobody could know the cost or the quality of what it extracted before paying for all of it.
- The scope is conversation time (the last event's `occurred_at`), not capture time: an export imported today is years old and is not "the last week". It is saved as a fixed start date, so new chats are always inside it. The progress line counts only chats inside the scope ("已检查 3 / 40 个聊天（2026-11-01 之后）").
- Migration `20261129000001` adds a nullable `since` to `memory_extraction_settings`. Existing rows read as "every chat", which is what they meant. The panel's on/off switch carries the saved scope, because a save without it means every chat.

### App: select across messages, copy a message or the whole conversation

- A mouse drag in a conversation now selects across paragraphs and messages. Each paragraph used to be its own selectable text (`MarkdownBody(selectable: true)`), so a drag stopped at every paragraph end. The text is now plain, and one `SelectionArea` around the conversation handles selection.
- Each question and answer has a copy button that copies its original Markdown, so code blocks survive the paste. The top bar has "复制整段对话", which first fetches the pages not yet scrolled to and then copies the title and every message with speaker and absolute time. It copies what is shown: only the dialogue, or the whole process when "查看过程" is on.

### Import: pick the vendor's whole zip

- The browser now takes the zip exactly as the vendor mailed it. Before uploading it copies out only the conversation files, as their compressed bytes with nothing decompressed, and leaves images and attachments on the machine. A real ChatGPT export of 275 MB, over the 256 MB upload ceiling, uploads as 21 MB. A zip with no conversations is refused with the names it holds, before anything is sent.
- ChatGPT exports since 2026-11 cut `conversations.json` into `conversations-000.json` … `conversations-NNN.json` (100 conversations each). The import now joins the pages, both in the browser import and in `cormex import`. A Claude export wrapped in one zip (`conversations-000.zip` inside) is opened too.
- ChatGPT voice-mode turns are imported. Their text lives only in `audio_transcription` parts, which were skipped: on a real export, 1704 transcriptions were dropped and 21 conversations came out empty. The parser is now `chatgpt_export@3`, so ChatGPT conversations imported earlier are projected again beside the old projection; nothing is deleted.

## 0.1.19 — 2026-11-08

### App: memory extraction moved to Settings

- The memory extraction panel (status, the full-extraction switch, progress, errors and retry, the model in use) is now at the top of **Settings**, above "模型配置", instead of taking the top of the memory page. The memory page opens on the categories and the search box; when it is empty, its centre still says extraction is off and where to turn it on.
- The panel's own "前往设置" button is gone: it now lives on the settings page.

### CLI: file ids left over from another device key heal themselves

- `state.db` remembers file ids by agent and path, not by which device key registered them. A machine that switched key and switched back kept ids the server refuses as another device's: in production, 38 Codex sessions got a 404 on their metadata on every sync, and an append to any of them would have been refused the same way.
- Such a refusal (404 "no such file for this device", 403 "belongs to another device", recognised by the server's own reason so an older server's missing route does not count) now forgets that file's local state. The transcript pass registers it again in the same run; the metadata pass on the next. Registering again verifies the archived bytes by prefix hash and adopts them, so nothing is uploaded twice.

### Deployment: local and Actions deployment share the same checks

- `just deploy` runs the Ansible deployment over SSH using connection details from environment variables. It deploys published images and verifies the running version, API routing, Web page and S3 backend.
- The production reference keeps the object store in its own stack and passes through the master encryption key and automatic-memory switches. Startup waits for the object store so an unavailable S3 service cannot silently leave uploads on the local filesystem.

## 0.1.18 — 2026-11-08

### 发布：本地构建也产出 Windows CLI

- `just build-local` 默认产出两个 x86_64 CLI：`x86_64-unknown-linux-musl` 与
  `x86_64-pc-windows-msvc`。Windows 目标在 Linux 上用 cargo-xwin 交叉编译，三元组与
  Actions 完全一致；需要 `clang-cl` 与 `llvm-lib`，首次构建从微软 CDN 取 CRT 与
  Windows SDK。二进制在 Linux 上不能执行，所以只做编译、打包与格式校验，**没有版本烟测**。
- aarch64 与 Apple 目标默认不再构建。`CORMEX_CLI_TARGETS` 是启用入口；单个目标可用
  `just build-cli-windows`、`just build-cli-windows-arm64`、`just build-cli`。macOS 目标
  仍需要 macOS 宿主 —— zig 只带 libc stub，不带 `Security` / `CoreFoundation`，
  Linux 上交叉构建会在链接期失败。

### 发布：镜像身份校验不再绑定经典 image store

- Docker 29 默认的 containerd 镜像存储下，镜像 ID 是 manifest digest，`docker save`
  会把 config 重写成 Docker schema，registry 侧又是第三个 digest。旧的
  `config digest == image_id` 两处等式只在经典 store 成立，于是本地构建能出候选却
  发不出去（`Docker config differs from reviewed image ID`）。
- 现在只比对两种 store 下都稳定的镜像 ID：载入后必须报出候选记录的 ID；推送前后各
  删除本地 tag、重拉 registry 副本再比对。归档字节仍由 `SHA256SUMS` 与
  `BUNDLE_SHA256` 钉住。

## 0.1.17 — 2026-11-07

### Capture: the same conversation is archived once, whatever happens

- A file the server has never seen under this device and path is offered the generations other files of the **same session and role** already hold. The client proves its bytes match one with a prefix hash (`POST /capture/generations/{id}/verify`) and follows it (`POST /capture/files/{id}/adopt`) instead of uploading the history again. Covered: reinstalling with a **new** device key, Codex moving a rollout into `archived_sessions/`, a renamed Claude Code project directory, a `.orphaned-…` set-aside copy, the same session on a second machine. Before, each of these uploaded every byte again and the projector showed the conversation twice.
- The client no longer resumes on an offset alone. After `state.db` is lost, and whenever the server answers 409 "I am ahead", it verifies the server's prefix against its own bytes first: identical → continue at the server's end; local is an older copy → send nothing; different bytes (the file was rewritten while nobody was watching) → a new generation, with the old one left intact. The old behaviour spliced a rewritten file onto the previous history without a word.
- Two machines appending to one session now append to one generation; the second machine's identical bytes are verified and skipped.
- New migration `20261127000001_capture_generation_adoptions.sql` (table `capture_generation_adoptions`, view `capture_current_generations`). Append-only; `ON DELETE CASCADE` so purge is unaffected.
- Not covered: conversations that were already duplicated before this change stay duplicated in the archive until a repair pass is run.

### Capture CLI: every agent directory, every run

- An agent's home used to be the **first** existing candidate: with `CODEX_HOME` (or `CLAUDE_CONFIG_DIR`, `PI_CODING_AGENT_DIR`, …) set, the default directory was not scanned at all, so a CLI with its own home and a desktop app on the default captured one of the two. Every existing home is now scanned: the variable's directory, the catalog's defaults, and the new `extra_homes` in `config.toml` (`extra_homes = { codex = ["/data/codex"] }`), each directory once.
- Files under the default location keep their logical paths. Files under any other home get a stable `@home-<hash>/` prefix derived from the directory, so the same relative path in two homes is two files, and a hook and a scheduled run name the same directory the same way. A home that was only captured through its variable before this change is registered again under the prefix; its bytes are recognised and adopted, not uploaded a second time (snapshot sources such as SQLite rows are uploaded once more).
- `cormex init` records the directories the environment variables point at in `extra_homes`. A hook inherits the agent's environment but a scheduler usually does not, so a home named only by a shell variable used to be captured only while the agent ran.
- `cormex doctor` has a `homes` check: a variable that points at no directory, a directory only a variable names, a configured home that was removed. `cormex detect` lists every home it scans.

### Capture CLI: Codex desktop metadata on every sync

- Titles, project assignments and pins from the Codex desktop app are read on every sync, from every Codex home, and sent only for sessions whose metadata changed (its hash is kept in `state.db`, schema 3). They used to be sent only by `cormex sync --codex-metadata`, which neither hooks nor the scheduler run, so a session renamed or moved into a project in Codex kept its old place in the archive. `--codex-metadata` now resends everything.
- A locked Codex database or a failed send is logged and retried on the next sync; it does not mark the sync as failed.

### Archive: import chat exports from the browser

- The archive has an **Import** button: pick a ChatGPT, Claude or Grok export zip, or a Gemini Takeout, and it is previewed (new / changed / already archived) before anything is written. `POST /archive/import/preview` and `POST /archive/import/run` take a file from `POST /import/upload`; the existing `/import/*` routes, which feed **memory**, are unchanged.
- Re-importing is idempotent: an unchanged conversation is skipped, a changed one gets a new snapshot generation with the old one kept, and a conversation deleted at the vendor stays archived. A full export imported every month writes only what changed. The same conversation is recognised whether it first arrived through the CLI or the browser.
- New export formats: **Grok** (`prod-grok-backend.json`, the account object) and **Gemini** (Takeout `MyActivity.json`). Neither vendor documents its format; both parsers follow the shape community tools describe and name the keys they saw when it does not match. Takeout does not record which prompts formed one conversation, so each prompt and answer is one session.
- Exports can be given as the zip the vendor sends; the CLI and the server pick the export file out of it the same way.
- Fixed in `cormex import`:
  - Claude conversations were stored under a **positional** id (`claude_export-<n>`), because the splitter read `id` and Claude names it `uuid`. A later export with one more conversation would have written each conversation into its neighbour's session. The splitter now uses the parser's own id rule (`identity()`), the one place it is written. Claude conversations already imported keep their old sessions; a re-import creates sessions under the real id beside them.
  - A changed conversation was appended to its previous version (two JSON documents in one file), and an unchanged re-run could append a second copy. Same bytes are now skipped and changed ones get a new generation.
  - Without `--platform`, every export was archived under `chatgpt_export`, so a Claude export then failed to parse. The detected platform is used. `--platform` also takes `grok` and `gemini`.

### Capture: five more agents

- New sources, each parser written against sessions the real CLI wrote (pi 1.5.0, omp 18.18.0, DeepSeek Harness 0.2.2-rc.1, Grok Build 1.0.77, Hermes Agent 0.19.0, run against a toy repository) and committed as fixtures:
  - **pi** (`~/.pi/agent/sessions`) and its fork **omp** (`~/.omp/agent/sessions`), one parser for both. omp does not read `PI_CODING_AGENT_DIR`: pi does, and reading it in both would capture the same files twice under two agents.
  - **DeepSeek Harness** (`~/.dsh/sessions/*/session-*/session.v*.jsonl.zstd`), an event log of appended zstd frames. Its runtime-context snapshot, also written as a `user/message`, is shown as system text, not as a second question.
  - **Grok Build** (`~/.grok/sessions/<cwd>/<id>/updates.jsonl`), the ACP update stream; streamed chunks are merged into one message, and the project comes from the percent-encoded cwd in the path.
  - **Hermes Agent** (`~/.hermes/state.db`, `messages` table), with the session's cwd carried into each row so it gets a project.
- Every catalogued agent now has a `provider` and a `kind` (`chat` / `code`); the catalog test refuses one without them, and refuses a chat source that is not import-only.
- `GET /archive/sources` lists the sources that have sessions, with name, maker, kind and count. The archive's source filter uses it: grouped by maker, with counts, and without the agents nobody on this server uses.
- The client now reads every frame of a zstd file. It asked the first frame for its size, so a log that appends a frame per write was captured up to the end of its first frame. A frame still being written is left for the next sync.

### Archive: events from one line were silently dropped

- A projected event's id came from the offset of its line. When a line produced several events — thinking, text and a tool call in one assistant record, or one message per row of a whole-document export — they shared an id and the insert kept only the first. Measured on the fixtures: a Gemini CLI session lost 107 of 138 events; every imported ChatGPT or Claude conversation kept only its first message; Cursor lost the tool call after a text block. The second and later events of a line now get an ordinal in their seed; the first keeps exactly the old id, so nothing already projected changes identity.
- To recover what was dropped, the affected parsers' versions are bumped (`gemini@2`, `cursor@3`, `cline@2`, `copilot_chat@2`, `chatgpt_export@2`, `claude_export@2`, `opencode@2`, `goose@2`). The projector builds a new projection for those sessions beside the old one; nothing is deleted. Expect a one-off round of projection work after deploying, and memory extraction may revisit those sessions. Claude Code, Codex, Copilot CLI, Qwen Code and Aider emit one event per line and are unchanged.

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
