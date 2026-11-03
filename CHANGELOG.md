# Changelog

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
