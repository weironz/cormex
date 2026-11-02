# Changelog

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
