#!/usr/bin/env bash
# The database half of the backup/restore drill, against **the production
# database**, on the host that runs it.
#
#   scripts/pg-restore-drill.sh [CONTAINER] [WORKDIR]
#   (defaults: cormex-postgres, /data/cormex-drill)
#   DUMP=/path/to/file.dump scripts/pg-restore-drill.sh   # restore an existing dump
#
# # What it does
#
#   1. `pg_dump --format=custom` of the live database, inside its own
#      container: one consistent snapshot, read-only, no lock beyond what
#      pg_dump takes. The file is kept in WORKDIR: the drill leaves a real,
#      verified backup behind.
#   2. Start a throwaway Postgres from the **same image**, with no network and
#      no published port, and `pg_restore` the file into it (--no-owner
#      --no-acl: the roles of the live cluster do not exist there).
#   3. Count the rows of every table in both, schema by schema, and compare.
#   4. Remove the throwaway container.
#
# # Why this exists on top of `cargo test --test backup_restore_drill`
#
# That test dumps and restores a database it built itself. It proves the
# procedure; it cannot prove that **this** deployment's backup restores. On
# 2026-11-08 the first run of this script found there was no backup of the
# production database at all: the backup container on the node backed up a
# different, 13 MB cluster, and every base backup in the backup directory was
# that cluster's (compare `SYSTEM_ID` in their meta.env with
# `select system_identifier from pg_control_system()`).
#
# # Reading the counts
#
# The live side is counted after the dump, so it can only have grown: the
# archive tables are append-only. "restored > live" or a table missing from
# the restore fails the drill; "restored < live" is reported as rows written
# since the snapshot.
#
# # Requires
#
# Docker on the database host, the database container running, and free space
# in WORKDIR for one compressed dump (about a third of the database size).
set -euo pipefail

container=${1:-cormex-postgres}
workdir=${2:-/data/cormex-drill}
drill=cormex-restore-drill-$$
stamp=$(date -u +%Y%m%dT%H%M%SZ)

image=$(docker inspect -f '{{.Config.Image}}' "$container")
db=$(docker exec "$container" printenv POSTGRES_DB)
user=$(docker exec "$container" printenv POSTGRES_USER)
mkdir -p "$workdir"
dump=${DUMP:-$workdir/$db-$stamp.dump}

cleanup() { docker rm -f "$drill" >/dev/null 2>&1 || true; }
trap cleanup EXIT

if [ -n "${DUMP:-}" ]; then
  echo "== restore the existing dump $dump"
  [ "$(dirname "$dump")" = "$workdir" ] || { echo "DUMP must be inside $workdir"; exit 2; }
else
  echo "== dump $db from $container ($image)"
  started=$(date +%s)
  docker exec "$container" sh -c "pg_dump -U '$user' -d '$db' --format=custom -f /tmp/drill.dump"
  docker cp "$container:/tmp/drill.dump" "$dump" >/dev/null
  docker exec "$container" rm -f /tmp/drill.dump
  echo "   $(du -h "$dump" | cut -f1) in $(( $(date +%s) - started ))s -> $dump"
  sha256sum "$dump" > "$dump.sha256"
fi

# Every user table's exact row count, as "schema.table count" lines
count_sql="DO \$\$ DECLARE r record; n bigint; BEGIN
  FOR r IN SELECT schemaname, tablename FROM pg_tables
           WHERE schemaname NOT IN ('pg_catalog','information_schema') ORDER BY 1,2 LOOP
    EXECUTE format('SELECT count(*) FROM %I.%I', r.schemaname, r.tablename) INTO n;
    RAISE NOTICE '% %', r.schemaname || '.' || r.tablename, n;
  END LOOP; END \$\$;"

echo "== restore into a throwaway container (no network)"
docker run -d --name "$drill" --network none -e POSTGRES_PASSWORD=drill \
  -e POSTGRES_DB="$db" -v "$workdir:/drill:ro" "$image" >/dev/null
# Not pg_isready alone: the image's entrypoint first runs a temporary server
# to create the database, then restarts. pg_isready answers yes during the
# temporary one, before the database exists, and the first version of this
# script restored nothing because of it ("database does not exist")
for _ in $(seq 120); do
  docker logs "$drill" 2>&1 | grep -q "PostgreSQL init process complete" \
    && docker exec "$drill" psql -U postgres -d "$db" -Atc "select 1" >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$drill" psql -U postgres -d "$db" -Atc "select 1" >/dev/null
started=$(date +%s)
# Errors are counted, not fatal: pg_restore exits non-zero for things like a
# COMMENT on an extension owned by another role, and the counts below are the
# verdict, not the exit code
docker exec "$drill" pg_restore -U postgres -d "$db" --no-owner --no-acl \
  "/drill/$(basename "$dump")" 2> "$workdir/restore-$stamp.log" || true
echo "   restored in $(( $(date +%s) - started ))s; $(grep -c 'error:' "$workdir/restore-$stamp.log" || true) pg_restore errors (log: $workdir/restore-$stamp.log)"

echo "== compare row counts"
docker exec "$drill" psql -U postgres -d "$db" -qc "$count_sql" 2>&1 \
  | sed -n 's/^NOTICE:  //p' | sort > "$workdir/restored-$stamp.txt"
docker exec "$container" psql -U "$user" -d "$db" -qc "$count_sql" 2>&1 \
  | sed -n 's/^NOTICE:  //p' | sort > "$workdir/live-$stamp.txt"

python3 - "$workdir/live-$stamp.txt" "$workdir/restored-$stamp.txt" <<'PY'
import sys
read = lambda p: dict(l.split() for l in open(p) if l.strip())
live, restored = read(sys.argv[1]), read(sys.argv[2])
missing = sorted(set(live) - set(restored))
grew = sorted(t for t in live if t in restored and int(restored[t]) < int(live[t]))
wrong = sorted(t for t in live if t in restored and int(restored[t]) > int(live[t]))
total_live = sum(int(v) for v in live.values())
total_rest = sum(int(restored[t]) for t in live if t in restored)
print(f"   {len(live)} tables, {total_rest:,} rows restored of {total_live:,} live")
for t in grew:
    print(f"   written since the snapshot: {t} {restored[t]} -> {live[t]}")
for t in missing:
    print(f"   MISSING from the restore: {t} ({live[t]} rows)")
for t in wrong:
    print(f"   MORE rows restored than live: {t} {restored[t]} > {live[t]}")
if missing or wrong:
    print("DRILL FAILED"); sys.exit(1)
print("DRILL PASSED")
PY
