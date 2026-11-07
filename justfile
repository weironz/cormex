# Cormex —— the memory service.
#
#   just up          bring up Postgres + object storage
#   just db-migrate  apply the migrations
#   just run         run cormexd in a host process
#   just serve       run the whole stack in containers (cormexd included)
#   just ci          run the checks that need no database
#   just ci-full     the same, plus the tests that need a database
#   just build-local stage a candidate; just release-local publishes that candidate

set shell := ["bash", "-uc"]
set windows-shell := ["bash", "-uc"]
set dotenv-load := true
set positional-arguments := true

default:
    @just --list

# ── Dependencies ────────────────────────────────────────

# Postgres (with pgvector) + RustFS. **cormexd is not included** -- during development it
# runs in a host process, where changing one line and restarting takes a second. For the
# whole thing containerized see `just serve`.
up:
    docker compose up -d postgres rustfs
    @echo "postgres :5432 · rustfs :9000"

down:
    docker compose down

# Apply both migration sets, in the order and against the schemas they require.
db-migrate:
    #!/usr/bin/env bash
    # **The tenant set and the global set are two separate sets**, and their version
    # tables live in separate schemas -- merging them into one makes creating a new
    # tenant run into `relation "users" already exists`, with the failure landing
    # halfway through account creation. See the header of migrations-global/.
    #
    # The global set runs against a schema named `cortex_auth`, not `cormex_auth`,
    # because the pinned migrations are fully qualified `cortex_auth.…` and cannot be
    # edited. The rename back is what leaves the deployment with the name it queries --
    # `Store::migrate_global` does the same thing in code, and
    # `crates/cormex-store/tests/ci_migration_order.rs` pins the sequence so the two
    # cannot drift.
    set -euo pipefail
    url="${DATABASE_URL:?DATABASE_URL is not set}"
    # `psql` is often only inside the Postgres container, while `sqlx` is a host tool.
    # Falling back rather than requiring both on the host: the machine that has a
    # database to migrate is usually the one where the client lives in the container.
    if command -v psql >/dev/null; then
      psql_() { psql "$url" "$@"; }
    else
      ct="${CORMEX_PG_CONTAINER:-cormex-postgres}"
      psql_() { docker exec -i "$ct" psql "$url" "$@"; }
    fi
    # ★ Rename **forward first**, exactly as `Store::migrate_global` does. Creating a
    #   fresh `cortex_auth` while `cormex_auth` already exists leaves two schemas and
    #   replays the whole global set into the new one -- the version table is inside the
    #   schema, so the migrator sees an empty schema and applies every migration again.
    #   Measured, having done it: `cortex_auth` ended up with a second copy of `users`,
    #   `device_keys` and the rest.
    psql_ -c 'ALTER SCHEMA cormex_auth RENAME TO cortex_auth' 2>/dev/null || true
    psql_ -c 'CREATE SCHEMA IF NOT EXISTS cortex_auth'
    sqlx migrate run --source migrations
    sqlx migrate run --source migrations-global \
      --database-url "$url?options=-csearch_path%3Dcortex_auth%2Cpublic"
    # Rename back so the deployment sees the name it queries
    psql_ -c 'ALTER SCHEMA cortex_auth RENAME TO cormex_auth' 2>/dev/null || true

# ── Running it ──────────────────────────────────────────

run:
    cargo run -p cormexd

# The whole stack (including the cormexd container). The first run has to compile it.
serve:
    docker compose up -d --build
    @echo "cormexd http://127.0.0.1:${CORMEXD_PORT:-8080}"

# ── Checks: shared by local builds and Actions ───────────

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

doc-check:
    RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc --workspace --no-deps --locked

test:
    cargo test --workspace --locked

# Keep the no-database capture coverage explicit, including the nonempty-test gate.
capture-check:
    #!/usr/bin/env bash
    set -euo pipefail
    unset DATABASE_URL
    for package in cormex-transcript cormex; do
      out=$(cargo test -p "$package" --lib --locked 2>&1) || { echo "$out"; exit 1; }
      echo "$out"
      grep -qE 'test result: ok\. [1-9][0-9]* passed' <<< "$out"
    done

eval-check:
    cargo run -p cormex-evals --locked -- validate
    cargo run -p cormex-evals --locked -- validate --sessions

pipeline-test:
    python3 -m unittest discover -s scripts/tests -v

# An already provisioned pgvector database. CI must not start a second compose stack.
test-db-ready:
    #!/usr/bin/env bash
    set -euo pipefail
    : "${DATABASE_URL:?DATABASE_URL is not set}"
    export CORMEX_SECRET_KEY="${CORMEX_SECRET_KEY:-0000000000000000000000000000000000000000000000000000000000000001}"
    cargo test --workspace --locked -- --include-ignored --test-threads=4

test-db: up
    #!/usr/bin/env bash
    set -euo pipefail
    for attempt in {1..30}; do
      if docker compose exec -T postgres pg_isready; then break; fi
      sleep 2
    done
    docker compose exec -T postgres pg_isready
    just db-migrate
    just test-db-ready

ci: fmt-check lint doc-check test capture-check eval-check pipeline-test

ci-full: ci test-db app-check
    @echo "ci-full: all checks passed, including database and Flutter tests"

app-check:
    cd app && flutter pub get && flutter analyze && flutter test

# ── Build and publish are independent choices ────────────
# Parameters are positional, never pasted into shell source. See docs/release.md.

# Build locally, then upload an immutable draft candidate (no registry credentials).
# Default CLI targets are x86_64 only: linux-musl plus windows-msvc (cross-compiled with
# cargo-xwin). Set CORMEX_CLI_TARGETS to a comma-separated list to include the aarch64 or
# Apple targets; the Apple ones only build on a macOS host with the SDK.
build-local tag:
    #!/usr/bin/env bash
    set -euo pipefail
    just _build "$1"
    just _stage "$1"

# Windows CLI for the local candidate. Builds with cargo-xwin (CRT and Windows SDK come
# from Microsoft's CDN on the first run) and packs the .zip; a binary cannot be executed
# on Linux, so no version smoke test runs for it.
build-cli-windows tag:
    python3 scripts/release.py build-cli "$1" x86_64-pc-windows-msvc

# Windows on ARM. Off by default; same cross-compile path as above.
build-cli-windows-arm64 tag:
    python3 scripts/release.py build-cli "$1" aarch64-pc-windows-msvc

# Any other single target. musl needs cargo-zigbuild, everything else the platform's SDK.
build-cli target tag:
    python3 scripts/release.py build-cli "$1" "$2"

# Start the six-target Actions build. Does not publish.
build-github tag:
    python3 scripts/release.py dispatch-build "$1"

# Publish only the existing draft pinned by the SHA256SUMS digest. Never compiles.
release-local tag checksum:
    just _publish "$1" "$2"

release-github tag checksum:
    python3 scripts/release.py dispatch-release "$1" "$2"

# The old one-command build-and-publish path is intentionally unavailable.
release version:
    @echo 'Use just build-local vX.Y.Z (or build-github), then just release-local vX.Y.Z BUNDLE_SHA256 (or release-github).' >&2
    @exit 1

_preflight tag:
    python3 scripts/release.py preflight "$1"

_build tag:
    #!/usr/bin/env bash
    set -euo pipefail
    just _preflight "$1"
    just ci-full
    python3 scripts/release.py build-cli-local "$1"
    just _build-images "$1"
    python3 scripts/release.py bundle "$1"

_build-cli tag target:
    python3 scripts/release.py build-cli "$1" "$2"

_build-images tag:
    python3 scripts/release.py build-images "$1"

# Actions requires all six CLI platforms; local bundling records its partial set.
_bundle tag:
    python3 scripts/release.py bundle "$1" --require-all-targets

_stage tag:
    python3 scripts/release.py stage "$1"

_verify tag checksum directory:
    python3 scripts/release.py verify "$1" "$2" "$3"

_publish tag checksum:
    python3 scripts/release.py publish "$1" "$2"
