# Cormex —— the memory service.
#
#   just up          bring up Postgres + object storage
#   just db-migrate  apply the migrations
#   just run         run cormexd in a host process
#   just serve       run the whole stack in containers (cormexd included)
#   just ci          run every check CI runs, locally
#   just ci-full     the same, plus the tests that need a database
#   just release     build and publish a release, locally

set shell := ["bash", "-uc"]
set dotenv-load := true

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

# ── Checks ──────────────────────────────────────────────
#
# ★ **These are the CI jobs, not an approximation of them.** GitHub Actions minutes are
#   exhausted (see the header of .github/workflows/ci.yml), so until the quota is restored
#   these recipes are the gate. That makes "just ci is green" the claim that matters, and
#   it means the two have to stay in step: a check added to the workflow and not here is a
#   check nobody runs.

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --locked

# The tests that need a database, with `--include-ignored`.
test-db: up
    #!/usr/bin/env bash
    set -euo pipefail
    sleep 2
    just db-migrate
    cargo test --workspace --locked -- --include-ignored --test-threads=4

# The pure-logic half: fmt, clippy, unit tests. Fast, no database needed.
ci: fmt-check lint test

# Everything CI runs: fmt, clippy, the database tests, and the Flutter client.
ci-full: fmt-check lint test-db app-check
    @echo "# --include-ignored is the whole point: without it the database tests show"
    @echo "# 'ignored' where they would show 'ok', and the two look alike at a glance"
    @echo
    @echo "ci-full: everything green"

# The Flutter client: analyze and test.
app-check:
    cd app && flutter analyze && flutter test

# ── Releasing ───────────────────────────────────────────
#
# ★ **This is the release pipeline, run locally.** The workflow in
#   .github/workflows/release.yml is the same sequence, and its `push: tags` trigger is
#   commented out while the Actions quota is exhausted. `just release` is what ships a
#   version until then.
#
#   The preflight comes first and is the same gate: a version mismatch or an inconsistent
#   lock file is a second to check and a quarter of an hour to discover at the end of a
#   build.

# Check that the version, the tag and the lock file agree. No build.
release-check version:
    #!/usr/bin/env bash
    set -euo pipefail
    v="{{version}}"
    [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
      || { echo "version '$v' is not X.Y.Z" >&2; exit 1; }
    python3 - "$v" <<'PY'
    import sys, tomllib
    want = sys.argv[1]
    got = tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"]
    if got != want:
        sys.exit(f"the tag is v{want}, but Cargo.toml's workspace version is {got}")
    print(f"versions agree: {got}")
    PY
    cargo metadata --locked --format-version 1 > /dev/null
    echo "Cargo.lock is consistent with the manifests"

# Build the CLI binaries into dist/. Linux targets always; the rest only on their own OS.
release-cli version:
    #!/usr/bin/env bash
    set -euo pipefail
    v="{{version}}"
    rm -rf dist && mkdir -p dist
    # ★ **This produces the Linux binaries on any machine, and the other four only on
    #   their own OS.** Cross-compiling to macOS needs the Apple SDK and to Windows needs
    #   MSVC; neither is a thing you install by accident, and a recipe that silently
    #   produced four of six artefacts would make a release look complete when it is not.
    #   So the skips are printed, and `release` warns loudly if the set is incomplete.
    built=0
    # The two Linux targets are musl: a glibc binary built here refuses to start on an
    # older distribution with `GLIBC_2.xx not found`, which the user meets on first run
    # with no way to tell it from a corrupt download
    for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
      rustup target add "$target" 2>/dev/null || true
      cargo zigbuild --release --locked -p cormex --target "$target"
      name="cormex-$v-$target"
      mkdir -p "dist/$name"
      cp "target/$target/release/cormex" "dist/$name/"
      cp README.md LICENSE "dist/$name/" 2>/dev/null || true
      tar -czf "dist/$name.tar.gz" -C dist "$name"
      rm -rf "dist/$name"
      built=$((built + 1))
    done
    # The native targets build with the host toolchain
    for target in x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
      case "$target" in
        *darwin*) [ "$(uname -s)" = Darwin ] || { echo "skipping $target: not on macOS" >&2; continue; } ;;
        *windows*) [ "$(uname -s)" = MINGW64_NT-* ] || [ "$(uname -s)" = MSYS_NT-* ] \
                     || { echo "skipping $target: not on Windows" >&2; continue; } ;;
      esac
      rustup target add "$target" 2>/dev/null || true
      cargo build --release --locked -p cormex --target "$target"
      name="cormex-$v-$target"
      mkdir -p "dist/$name"
      bin="target/$target/release/cormex"
      [ -f "$bin.exe" ] && bin="$bin.exe"
      cp "$bin" "dist/$name/"
      cp README.md LICENSE "dist/$name/" 2>/dev/null || true
      case "$target" in
        *windows*) (cd dist && zip -qr "$name.zip" "$name") ;;
        *)         tar -czf "dist/$name.tar.gz" -C dist "$name" ;;
      esac
      rm -rf "dist/$name"
      built=$((built + 1))
    done
    echo "built $built of 6 targets"
    # The binary has to answer `--version` on the platform it was built for
    out=$(./target/x86_64-unknown-linux-musl/release/cormex --version)
    echo "$out"
    echo "$out" | grep -qF "$v" \
      || { echo "the binary reports a different version than $v" >&2; exit 1; }
    (cd dist && sha256sum * > SHA256SUMS && cat SHA256SUMS)

# Build both images and smoke-test them before pushing.
release-images version:
    #!/usr/bin/env bash
    # The smoke tests run **before** the push, because a broken image that is already in
    # the registry cannot be withdrawn.
    set -euo pipefail
    v="{{version}}"
    docker build -f scripts/docker/Dockerfile.cormexd -t "cormex:$v" .
    docker build -f scripts/docker/Dockerfile.web -t "cormex-web:$v" .
    # `--version` is the one code path that runs with no dependencies, and what it proves
    # is that dynamic linking works -- the glibc-mismatch kind of failure blows up here
    # instead of when the container starts in production
    out=$(docker run --rm "cormex:$v" --version)
    echo "$out"
    echo "$out" | grep -qF "$v" || { echo "the image reports a different version" >&2; exit 1; }
    # Both migration sets travel with the image: the deployment machine has no Rust
    # toolchain, and `just prod-migrate` runs them inside the container
    docker run --rm --entrypoint /bin/sh "cormex:$v" -c \
      'test -d /opt/cormex/migrations && test -d /opt/cormex/migrations-global' \
      || { echo "the migration directories are missing from the image" >&2; exit 1; }
    # The web image: healthy, a real document at /, and the API prefix survived the compile
    docker run -d --name cormex-web-smoke -p 8099:80 "cormex-web:$v"
    trap 'docker rm -f cormex-web-smoke >/dev/null 2>&1 || true' EXIT
    for _ in $(seq 1 30); do
      [ "$(docker inspect -f '{{{{.State.Health.Status}}}}' cormex-web-smoke)" = healthy ] && break
      sleep 2
    done
    [ "$(docker inspect -f '{{{{.State.Health.Status}}}}' cormex-web-smoke)" = healthy ] \
      || { docker logs cormex-web-smoke; echo "the web container never became healthy" >&2; exit 1; }
    curl -fsS http://127.0.0.1:8099/ | grep -qi '<title>' \
      || { echo "/ did not return an HTML document" >&2; exit 1; }
    curl -fsS http://127.0.0.1:8099/main.dart.js -o /tmp/main.js
    grep -qF '"/api"' /tmp/main.js || grep -qF "'/api'" /tmp/main.js \
      || { echo "the API prefix /api is not in the bundle" >&2; exit 1; }

# Tag both images for both registries and push them.
release-push version:
    #!/usr/bin/env bash
    # ★ `--provenance=false` is required, not a preference: buildx attaches a provenance
    #   attestation by default, which makes the result an OCI index containing an empty
    #   descriptor of type `application/vnd.oci.empty.v1+json`. ACR does not recognise
    #   that media type and rejects the push with `denied: unknown manifest class`.
    set -euo pipefail
    v="{{version}}"
    : "${ACR_USERNAME:?ACR_USERNAME is not set}"
    : "${ACR_PASSWORD:?ACR_PASSWORD is not set}"
    : "${DOCKERHUB_USERNAME:?DOCKERHUB_USERNAME is not set}"
    : "${DOCKERHUB_TOKEN:?DOCKERHUB_TOKEN is not set}"
    echo "$DOCKERHUB_TOKEN" | docker login -u "$DOCKERHUB_USERNAME" --password-stdin
    echo "$ACR_PASSWORD" | docker login -u "$ACR_USERNAME" --password-stdin \
      registry.cn-shenzhen.aliyuncs.com
    # Only the version is tagged, never `latest`: production pins a version and has to
    # come back to the same one after a restart, and a drifting tag turns "which version
    # is running" into a question nobody can settle
    for img in cormex cormex-web; do
      docker tag "$img:$v" "willdockerhub/$img:v$v"
      docker tag "$img:$v" "registry.cn-shenzhen.aliyuncs.com/willspace/$img:v$v"
      docker push "willdockerhub/$img:v$v"
      docker push "registry.cn-shenzhen.aliyuncs.com/willspace/$img:v$v"
    done
    # What gets pulled back is the ACR copy, not the local one: the local image has never
    # made the push/pull round trip, so manifest-level problems are invisible on it
    docker pull "registry.cn-shenzhen.aliyuncs.com/willspace/cormex:v$v"
    docker run --rm "registry.cn-shenzhen.aliyuncs.com/willspace/cormex:v$v" --version \
      | grep -qF "$v" || { echo "the pushed image reports a different version" >&2; exit 1; }

# Publish the CLI artefacts to npm (skipped when NPM_TOKEN is unset).
release-npm version:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "${NPM_TOKEN:-}" ]; then
      echo "NPM_TOKEN is not set: skipping the npm publish (install.sh is the primary path)"
      exit 0
    fi
    echo "//registry.npmjs.org/:_authToken=${NPM_TOKEN}" > ~/.npmrc
    node npm/publish.mjs "{{version}}" dist

# The whole release: preflight, CLI, images, smoke tests, push, npm.
release version:
    #!/usr/bin/env bash
    # Everything is built and smoke-tested **before anything is pushed**: a release that
    # fails halfway leaves the registry with an image nobody can withdraw.
    set -euo pipefail
    just release-check {{version}}
    just release-cli {{version}}
    # ★ A partial CLI set is a **warning, not a stop**, and the distinction is deliberate.
    #   The images are the product; the CLI is how capture agents are installed. A Linux
    #   machine can build two of the six binaries, and refusing to release the server
    #   because a laptop cannot cross-compile to macOS would block the thing that matters
    #   on the thing that does not. What must not happen is a *silent* partial set -- so
    #   it says so, loudly, and names what is missing.
    n=$(ls dist/*.tar.gz dist/*.zip 2>/dev/null | wc -l)
    if [ "$n" -lt 6 ]; then
      echo
      echo "WARNING: only $n of 6 CLI binaries were built on this machine."
      echo "  The release will publish a partial set. To get all six, either run"
      echo "  \`just release-cli\` on macOS and Windows too, or restore the tag trigger in"
      echo "  .github/workflows/release.yml and let CI build them."
      echo
    fi
    just release-images {{version}}
    just release-push {{version}}
    just release-npm {{version}}
    echo
    echo "released v{{version}}"
