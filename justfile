# Cormex —— 记忆服务。
#
#   just up          起 Postgres + 对象存储
#   just db-migrate  应用 migration
#   just run         在宿主进程里跑 cortexd
#   just serve       整套跑在容器里（含 cortexd）
#   just ci          本地跑一遍 CI 的全部检查

set shell := ["bash", "-uc"]
set dotenv-load := true

default:
    @just --list

# ── 依赖 ────────────────────────────────────────────────

# Postgres（带 pgvector）+ RustFS。**不含 cortexd** —— 开发时它在宿主
# 进程里跑，改一行重启是秒级的。整套容器化见 `just serve`。
up:
    docker compose up -d postgres rustfs
    @echo "postgres :5432 · rustfs :9000"

down:
    docker compose down

# 应用 migration。**租户那套与全局那套是两份**，版本表也各在各的 schema
# 里 —— 混成一套会让新租户建号时撞上 `relation "users" already exists`，
# 而失败点在建号中途。见 migrations-global/ 的文件头。
db-migrate:
    sqlx migrate run --source migrations
    sqlx migrate run --source migrations-global

# ── 跑起来 ──────────────────────────────────────────────

run:
    cargo run -p cortexd

# 整套（含 cortexd 容器）。第一次要编一遍。
serve:
    docker compose up -d --build
    @echo "cortexd http://127.0.0.1:${CORTEXD_PORT:-8080}"

# ── 检查 ────────────────────────────────────────────────

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

# 要数据库的那些集成测试。没有 Postgres 时它们会自己跳过，
# 而「跳过」与「通过」在 CI 输出里长得一样 —— 所以单列一条。
test-db: up
    sleep 2 && just db-migrate && cargo test --workspace -- --include-ignored

ci: fmt-check lint test
