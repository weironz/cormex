#!/usr/bin/env bash
# The object-store half of the backup/restore drill (P6-3), against **a real
# RustFS volume**.
#
#   scripts/rustfs-volume-drill.sh
#
# # Why this exists on top of the two Rust tests
#
# `crates/cormex-blob/tests/backup_restore_objects.rs` proves that a copy of a
# directory tree is a working `LocalFsBlobStore`. It cannot prove that a copy of
# a **RustFS volume** is a working RustFS: the volume's layout is RustFS's own
# (an object is a directory holding an `xl.meta`, not a plain file), and a
# restore that unpacked it correctly still has to be read back **by RustFS** to
# mean anything.
#
# So this does the whole thing end to end, with the real image:
#
#   1. write an object through the S3 API into the running store
#   2. `tar` the volume, the way `docs/operations.md` says to
#   3. unpack it into a **new** volume
#   4. start a **second** RustFS on that volume, on a different port
#   5. read the object back from the restored store and compare byte for byte
#
# # Why the second container rather than swapping volumes
#
# The running container holds the volume. Stopping it to swap would make the
# drill destructive on a machine somebody is using, and the whole point is that
# it can be run at any time. A second container on the restored volume answers
# the same question — "does RustFS read this copy" — without touching the first.
#
# # What it does not cover
#
# The database. `cargo test -p cormex-store --test backup_restore_drill` covers
# that half. Neither drill alone is a restore; a real one needs both, and the
# row counts only line up if the objects are there too.
#
# # Requires
#
# Docker, and the development stack (`just up`) with its RustFS running. The
# credentials come from the running container's own environment rather than
# being typed in, because a drill that runs with different credentials from the
# deployment is not drilling the deployment.

set -euo pipefail

SRC_CT="${CORMEX_RUSTFS_CONTAINER:-cormex-rustfs}"
SRC_VOL="${CORMEX_RUSTFS_VOLUME:-cormex_rustfs_data}"
NEW_VOL="cormex_rustfs_drill_restored"
NEW_CT="cormex-rustfs-drill"
NEW_PORT="${CORMEX_DRILL_PORT:-19000}"
BAK="$(mktemp -t rustfs-drill-XXXXXX).tar.gz"
KEY="drill/$(date +%s)/object.bin"
BODY="the bytes that have to survive a restore"

cleanup() {
  docker rm -f "$NEW_CT" >/dev/null 2>&1 || true
  docker volume rm "$NEW_VOL" >/dev/null 2>&1 || true
  rm -f "$BAK" 2>/dev/null || true
}
trap cleanup EXIT

# **The credentials come from the running container.** A drill that runs with
# different credentials from the deployment is not drilling the deployment.
ACCESS="$(docker exec "$SRC_CT" printenv RUSTFS_ACCESS_KEY)"
SECRET="$(docker exec "$SRC_CT" printenv RUSTFS_SECRET_KEY)"
BUCKET="$(docker exec "$SRC_CT" printenv RUSTFS_BUCKET 2>/dev/null || echo cortex-blobs)"

aws() {
  docker run --rm -i --network host \
    -e AWS_ACCESS_KEY_ID="$ACCESS" -e AWS_SECRET_ACCESS_KEY="$SECRET" \
    -e AWS_DEFAULT_REGION=us-east-1 \
    amazon/aws-cli:latest --endpoint-url "$1" "${@:2}"
}

echo "== 1. write an object through the S3 API =="
aws http://127.0.0.1:9000 s3 cp - "s3://$BUCKET/$KEY" <<<"$BODY"
echo "   wrote s3://$BUCKET/$KEY"

echo "== 2. back up the volume =="
docker run --rm -v "$SRC_VOL":/data:ro -v "$(dirname "$BAK")":/backup alpine \
  tar czf "/backup/$(basename "$BAK")" -C /data .
echo "   $(du -h "$BAK" | cut -f1) archive"

echo "== 3. restore into a new volume =="
docker volume create "$NEW_VOL" >/dev/null
docker run --rm -v "$NEW_VOL":/data -v "$(dirname "$BAK")":/backup alpine \
  tar xzf "/backup/$(basename "$BAK")" -C /data
echo "   unpacked"

echo "== 4. start a second RustFS on the restored volume =="
docker run -d --name "$NEW_CT" -p "127.0.0.1:$NEW_PORT:9000" \
  -e RUSTFS_VOLUMES=/data -e RUSTFS_UNSAFE_BYPASS_DISK_CHECK=true \
  -e RUSTFS_ADDRESS=0.0.0.0:9000 -e RUSTFS_CONSOLE_ENABLE=false \
  -e RUSTFS_ACCESS_KEY="$ACCESS" -e RUSTFS_SECRET_KEY="$SECRET" \
  -e RUSTFS_DURABILITY_MODE=strict \
  -v "$NEW_VOL":/data rustfs/rustfs:latest >/dev/null

for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$NEW_PORT/health" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS "http://127.0.0.1:$NEW_PORT/health" >/dev/null || {
  echo "FAIL: the restored RustFS did not come up" >&2
  docker logs "$NEW_CT" 2>&1 | tail -20 >&2
  exit 1
}

echo "== 5. read the object back from the restored store =="
GOT="$(aws "http://127.0.0.1:$NEW_PORT" s3 cp "s3://$BUCKET/$KEY" -)"

if [ "$GOT" != "$BODY" ]; then
  echo "FAIL: the restored object does not match" >&2
  echo "  expected: $BODY" >&2
  echo "  got:      $GOT" >&2
  exit 1
fi
echo "   byte-for-byte match"

echo
echo "drill passed"