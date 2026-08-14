# Cormex —— the memory service.
#
#   just up          bring up Postgres + object storage
#   just db-migrate  apply the migrations
#   just run         run cortexd in a host process
#   just serve       run the whole stack in containers (cortexd included)
#   just ci          run every check CI runs, locally

set shell := ["bash", "-uc"]
set dotenv-load := true

default:
    @just --list

# ── Dependencies ────────────────────────────────────────

# Postgres (with pgvector) + RustFS. **cortexd is not included** -- during development it
# runs in a host process, where changing one line and restarting takes a second. For the
# whole thing containerized see `just serve`.
up:
    docker compose up -d postgres rustfs
    @echo "postgres :5432 · rustfs :9000"

down:
    docker compose down

# Apply the migrations. **The tenant set and the global set are two separate sets**, and
# their version tables live in separate schemas -- merging them into one makes creating a
# new tenant run into `relation "users" already exists`, with the failure landing halfway
# through account creation. See the header of migrations-global/.
db-migrate:
    sqlx migrate run --source migrations
    sqlx migrate run --source migrations-global

# ── Running it ──────────────────────────────────────────

run:
    cargo run -p cortexd

# The whole stack (including the cortexd container). The first run has to compile it.
serve:
    docker compose up -d --build
    @echo "cortexd http://127.0.0.1:${CORTEXD_PORT:-8080}"

# ── Checks ──────────────────────────────────────────────

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

# The integration tests that need a database. They skip themselves when there is no
# Postgres, and "skipped" looks exactly like "passed" in CI output -- hence its own recipe.
test-db: up
    sleep 2 && just db-migrate && cargo test --workspace -- --include-ignored

ci: fmt-check lint test
