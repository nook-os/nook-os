#!/usr/bin/env bash
#
# Squash a service's migration set into a single canonical 0001 (MAIN-235).
#
# The repo's only previous squash (19 -> 1) was a hand-run, once-only operation,
# and it produced the prod near-miss recorded in CLAUDE.md: the ledger was
# re-stamped by hand BEFORE the image carrying the squash shipped, so the next
# restart would have re-applied 0002..0019 against a schema that already had
# them. This script is the repeatable replacement. Two properties make it safe
# where that was not:
#
#   1. The new 0001 is GENERATED FROM REAL APPLICATION, never hand-edited. We
#      apply every current migration to a virgin database and dump what they
#      actually produced — so the file is what the set does, not what someone
#      believed it does.
#   2. It emits a MANIFEST, and the re-stamp is the running binary's job at boot
#      (see crates/nook-db/src/restamp.rs). The image that carries the squash
#      carries its own re-stamp; there is no two-step ordering to get wrong.
#
# It then verifies the result three ways — the same three checks the blessed
# 19->1 squash used by hand, mechanized here:
#
#   (a) schema diff  — virgin-from-N vs virgin-from-the-new-1 must be identical
#   (b) seed rows    — per-table row counts must be identical
#   (c) real boot    — left to `./test.sh` (the caller runs it; see --help)
#
# Nothing is written to the repo unless --apply is passed AND (a) and (b) pass.
#
# Usage:
#   scripts/squash-migrations.sh --set control [--apply]
#   scripts/squash-migrations.sh --set chat    [--apply]
#
# Requires `psql` and `pg_dump` on PATH and a LOCAL DATABASE_URL — run it inside
# the compose Postgres container, which has both:
#
#   docker compose exec -T postgres sh -lc 'cd /repo && scripts/squash-migrations.sh --set control --apply'
#
# ...or with postgresql-client installed on the host.
set -euo pipefail

SET=""
APPLY=0
KEEP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --set) SET="${2:-}"; shift 2 ;;
    --apply) APPLY=1; shift ;;
    --keep) KEEP=1; shift ;;   # leave the scratch databases for debugging
    -h|--help) sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

case "$SET" in
  control) MIG_DIR="crates/nook-control/migrations"; SCHEMA="public"; NEW_NAME="0001_init.sql" ;;
  chat)    MIG_DIR="crates/nook-chat/migrations";    SCHEMA="chat";   NEW_NAME="0001_chat_init.sql" ;;
  *) echo "refusing: --set must be 'control' or 'chat'" >&2; exit 2 ;;
esac

# --- Guard 1: never against production. ---------------------------------------
# A squash rewrites the ledger. The one thing that must never happen by accident.
if [[ "${APP_ENV:-}" == "production" ]]; then
  echo "refusing: APP_ENV=production. This rewrites a migration ledger." >&2
  exit 1
fi

# --- Guard 2: a LOCAL database only. ------------------------------------------
# Same rule as dev-db-heal.sh, and deliberately strict: better to refuse a
# legitimate dev URL than to touch something that is not.
: "${DATABASE_URL:?set DATABASE_URL to a local Postgres}"
host="$(printf '%s' "$DATABASE_URL" | sed -E 's#^[a-z+]+://([^/@]*@)?([^:/?]+).*#\2#')"
case "$host" in
  localhost|127.0.0.1|::1|postgres|db) ;;
  *) echo "refusing: DATABASE_URL host '$host' is not local or a compose service name." >&2
     exit 1 ;;
esac

for tool in psql pg_dump base64; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "refusing: '$tool' not found on PATH. Run this inside the compose" >&2
    echo "Postgres container (docker compose run --rm -v \"\$PWD:/repo\" -w /repo" >&2
    echo "postgres bash scripts/squash-migrations.sh ...), which has them." >&2
    exit 1
  }
done

[[ -d "$MIG_DIR" ]] || { echo "no such migration directory: $MIG_DIR" >&2; exit 1; }

# Everything hangs off the server in DATABASE_URL; the scratch databases are
# siblings of it, named so a stray one is obviously disposable.
ADMIN_URL="${DATABASE_URL%/*}/postgres"
STAMP="$$"
FROM_N="nook_squash_from_n_$STAMP"
FROM_1="nook_squash_from_1_$STAMP"
WORK="$(mktemp -d)"

cleanup() {
  if [[ $KEEP -eq 1 ]]; then
    echo "--keep: leaving $FROM_N / $FROM_1 and $WORK"
    return
  fi
  psql "$ADMIN_URL" -qc "DROP DATABASE IF EXISTS $FROM_N" >/dev/null 2>&1 || true
  psql "$ADMIN_URL" -qc "DROP DATABASE IF EXISTS $FROM_1" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

say() { printf '\033[1m▸ %s\033[0m\n' "$*"; }
die() { printf '\033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# The migration files in version order. sqlx orders by the numeric prefix, and
# so must we — a plain glob sort would put 0010 before 0002 only if the widths
# disagreed, but be explicit rather than lucky.
mapfile -t FILES < <(find "$MIG_DIR" -maxdepth 1 -name '*.sql' -print | sort -t/ -k2 -V)
[[ ${#FILES[@]} -gt 0 ]] || die "no .sql migrations in $MIG_DIR"

if [[ ${#FILES[@]} -eq 1 ]]; then
  say "already a single migration (${FILES[0]}) — nothing to squash."
  exit 0
fi

say "squashing ${#FILES[@]} migrations in $MIG_DIR (schema: $SCHEMA)"

# `sqlx` derives a migration's version and description from its filename, and
# its checksum from the file's bytes (SHA-384 — verified against a live ledger).
# Reproducing that here is what lets the manifest name the exact rows the
# re-stamp will collapse.
version_of() { basename "$1" | sed -E 's/^0*([0-9]+)_.*/\1/'; }
description_of() { basename "$1" .sql | sed -E 's/^[0-9]+_//; s/_/ /g'; }

# SHA-384 of the file's bytes. `sha384sum` is the obvious way and is used when
# present, but the container this script is documented to run in (postgres:16
# -alpine) ships neither it nor openssl — and Postgres itself has `sha384()`,
# which is the same function sqlx's ledger records. The fallback keeps the
# documented invocation working instead of quietly requiring a different image.
if command -v sha384sum >/dev/null 2>&1; then
  checksum_of() { sha384sum "$1" | cut -d' ' -f1; }
elif command -v openssl >/dev/null 2>&1; then
  checksum_of() { openssl dgst -sha384 "$1" | awk '{print $NF}'; }
else
  checksum_of() {
    base64 < "$1" | tr -d '\n' > "$WORK/.b64"
    # The query goes in on STDIN, not -c: psql expands `:'var'` only when
    # reading a script, never in a -c command string.
    printf "SELECT encode(sha384(decode(:'b64', 'base64')), 'hex');\n" \
      | psql "$ADMIN_URL" -tAXq -v b64="$(cat "$WORK/.b64")" -f -
  }
fi

fresh_db() {
  psql "$ADMIN_URL" -qc "DROP DATABASE IF EXISTS $1" >/dev/null
  psql "$ADMIN_URL" -qc "CREATE DATABASE $1" >/dev/null
}

# A pg_dump into something sqlx can execute.
#
# sqlx hands a migration's text straight to the driver — it is not psql — so
# three things pg_dump emits have to go, and each of them is a real failure, not
# tidiness:
#
#   \restrict / \unrestrict   psql meta-commands (pg16.12+). A syntax error to
#                             the driver, so the migration would never apply.
#   set_config('search_path', '', false)
#                             blanks the search_path for the REST of the
#                             session, so the migrator's own ledger write
#                             afterwards can no longer resolve its table.
#   CREATE SCHEMA <s>;        collides on a database that already has it. Made
#                             IF NOT EXISTS, matching the house idempotency rule.
sql_from_dump() {
  grep -v '^--' "$1" \
    | grep -v '^$' \
    | grep -v '^\\restrict' \
    | grep -v '^\\unrestrict' \
    | grep -v "^SELECT pg_catalog.set_config('search_path'" \
    | sed -E "s/^CREATE SCHEMA ($SCHEMA);/CREATE SCHEMA IF NOT EXISTS \\1;/"
}

# Apply one .sql file to a database, inside the target schema.
apply_sql() {
  local db="$1" file="$2"
  psql "${DATABASE_URL%/*}/$db" -v ON_ERROR_STOP=1 -q \
    -c "CREATE SCHEMA IF NOT EXISTS $SCHEMA" \
    -c "SET search_path TO $SCHEMA" \
    -f "$file" >/dev/null
}

# --- 1. Virgin database from the CURRENT set. ---------------------------------
say "building a virgin database from all ${#FILES[@]} migrations"
fresh_db "$FROM_N"
for f in "${FILES[@]}"; do
  apply_sql "$FROM_N" "$f" || die "migration failed to apply: $f"
done

# --- 2. Dump what they actually produced — this IS the new 0001. --------------
say "dumping the result (schema + seed rows) as the new $NEW_NAME"
pg_dump --schema-only --no-owner --no-privileges --schema="$SCHEMA" \
  "${DATABASE_URL%/*}/$FROM_N" > "$WORK/schema.sql" || die "pg_dump --schema-only failed"

# Seed rows: whatever the migrations inserted. `--data-only --inserts` keeps the
# result readable and replayable, and excluding the ledger is essential — the
# new file must not carry the old set's `_sqlx_migrations` rows.
pg_dump --data-only --inserts --no-owner --no-privileges --schema="$SCHEMA" \
  --exclude-table="$SCHEMA._sqlx_migrations" \
  "${DATABASE_URL%/*}/$FROM_N" > "$WORK/data.sql" || die "pg_dump --data-only failed"

{
  cat <<EOF
-- NookOS canonical schema — GENERATED, DO NOT HAND-EDIT.
--
-- Produced by scripts/squash-migrations.sh --set $SET from the $((${#FILES[@]})) migrations
-- that preceded it, by applying every one of them to a virgin database and
-- dumping what they actually produced. It is what that set does, not what
-- anyone believed it does.
--
-- Existing databases are NOT re-migrated from this file. The binary that ships
-- it collapses a matching pre-squash ledger to this single row at boot, in one
-- transaction (crates/nook-db/src/restamp.rs, manifest in squash-manifest.txt).
--
-- The append-only rule still holds: the next schema change is a NEW numbered
-- file, never an edit to this one.
EOF
  sql_from_dump "$WORK/schema.sql"
  echo
  echo "-- Seed rows the original migrations inserted."
  sql_from_dump "$WORK/data.sql"
} > "$WORK/$NEW_NAME"

# --- 3. Virgin database from the SQUASHED file. -------------------------------
say "building a second virgin database from the squashed file"
fresh_db "$FROM_1"
apply_sql "$FROM_1" "$WORK/$NEW_NAME" || die "the generated $NEW_NAME does not apply cleanly"

# --- 4. Verification (a): the schemas must be identical. ----------------------
say "verify (a): schema diff"
# Compare the SQL, not pg_dump's packaging. `\restrict` carries a fresh random
# nonce on every invocation, so an unnormalized diff is never empty and the
# check would be worthless — it would fail identical schemas and, worse, teach
# whoever runs it to ignore the output.
dump_norm() {
  pg_dump --schema-only --no-owner --no-privileges --schema="$SCHEMA" "${DATABASE_URL%/*}/$1" \
    | sql_from_dump /dev/stdin | sort
}
dump_norm "$FROM_N" > "$WORK/a.txt"
dump_norm "$FROM_1" > "$WORK/b.txt"
if ! diff -u "$WORK/a.txt" "$WORK/b.txt" > "$WORK/schema.diff"; then
  head -40 "$WORK/schema.diff" >&2
  die "schema diff is NOT empty (full diff: $WORK/schema.diff) — refusing to write"
fi
echo "  identical"

# A verification that cannot fail is not a verification. Prove this one can see
# a difference before trusting the "identical" above: add one table the real
# migrations never created, and require the diff to notice. Cheap (one CREATE
# TABLE against a scratch database) and it runs every time, so a future edit
# that neuters the comparison — normalizing too hard, say — is caught here
# rather than by whoever deploys the squash.
say "verify (a'): the schema diff can actually detect a difference"
psql "${DATABASE_URL%/*}/$FROM_1" -q -c \
  "CREATE TABLE $SCHEMA.squash_selftest_canary (x int)" >/dev/null
dump_norm "$FROM_1" > "$WORK/b_canary.txt"
if diff -q "$WORK/a.txt" "$WORK/b_canary.txt" >/dev/null; then
  die "the schema diff did NOT notice an injected table — the check is broken, \
refusing to trust its verdict"
fi
psql "${DATABASE_URL%/*}/$FROM_1" -q -c \
  "DROP TABLE $SCHEMA.squash_selftest_canary" >/dev/null
# …and the databases must be identical again, or the self-test left residue.
dump_norm "$FROM_1" > "$WORK/b.txt"
diff -q "$WORK/a.txt" "$WORK/b.txt" >/dev/null \
  || die "the self-test left the scratch database changed — refusing to write"
echo "  detected (and reverted)"

# --- 5. Verification (b): seed rows must match, table by table. ---------------
say "verify (b): seed-row counts"
counts() {
  # count(*) only: pg_stat's n_live_tup is an async-sampled counter, so two
  # freshly-built databases can disagree on it purely on stats-flush timing —
  # a flaky refusal in the one check that must be deterministic (MAIN-308).
  psql "${DATABASE_URL%/*}/$1" -tA -c "
    SELECT string_agg(t || '=' || c, E'\n' ORDER BY t) FROM (
      SELECT table_name AS t,
             (xpath('/row/c/text()',
                    query_to_xml(format('SELECT count(*) AS c FROM %I.%I', table_schema, table_name),
                                 false, true, '')))[1]::text::bigint AS c
        FROM information_schema.tables
       WHERE table_schema = '$SCHEMA' AND table_type = 'BASE TABLE'
         AND table_name <> '_sqlx_migrations'
    ) s"
}
counts "$FROM_N" > "$WORK/ca.txt"
counts "$FROM_1" > "$WORK/cb.txt"
if ! diff -u "$WORK/ca.txt" "$WORK/cb.txt" > "$WORK/counts.diff"; then
  cat "$WORK/counts.diff" >&2
  die "seed-row counts differ — refusing to write"
fi
echo "  identical ($(grep -c . "$WORK/ca.txt") tables)"

# The check must be able to FAIL, proven the same way verification (a) proves
# the schema diff: the same empty table on both sides, one extra row on one —
# a genuine seed-row difference and nothing else.
psql "${DATABASE_URL%/*}/$FROM_N" -q -c \
  "CREATE TABLE $SCHEMA.squash_selftest_rows (x int)" >/dev/null
psql "${DATABASE_URL%/*}/$FROM_1" -q -c \
  "CREATE TABLE $SCHEMA.squash_selftest_rows (x int)" >/dev/null
psql "${DATABASE_URL%/*}/$FROM_1" -q -c \
  "INSERT INTO $SCHEMA.squash_selftest_rows VALUES (1)" >/dev/null
counts "$FROM_N" > "$WORK/ca_canary.txt"
counts "$FROM_1" > "$WORK/cb_canary.txt"
if diff -q "$WORK/ca_canary.txt" "$WORK/cb_canary.txt" >/dev/null; then
  die "the seed-row check did NOT notice an injected row — the check is broken, \
refusing to trust its verdict"
fi
psql "${DATABASE_URL%/*}/$FROM_N" -q -c \
  "DROP TABLE $SCHEMA.squash_selftest_rows" >/dev/null
psql "${DATABASE_URL%/*}/$FROM_1" -q -c \
  "DROP TABLE $SCHEMA.squash_selftest_rows" >/dev/null
# …and the counts must read exactly as before, or the self-test left residue.
counts "$FROM_N" > "$WORK/ca2.txt"
counts "$FROM_1" > "$WORK/cb2.txt"
{ diff -q "$WORK/ca.txt" "$WORK/ca2.txt" >/dev/null \
    && diff -q "$WORK/cb.txt" "$WORK/cb2.txt" >/dev/null; } \
  || die "the self-test left the scratch databases changed — refusing to write"
echo "  detected (and reverted)"

# --- 6. The manifest: exactly which ledger rows the re-stamp may collapse. ----
NEW_CHECKSUM="$(checksum_of "$WORK/$NEW_NAME")"
NEW_VERSION="$(version_of "$NEW_NAME")"
{
  echo "# NookOS migration squash manifest — GENERATED by scripts/squash-migrations.sh"
  echo "#"
  echo "# 'new' is the single row a matching pre-squash ledger collapses to."
  echo "# 'old' lines are that ledger, exactly: the re-stamp fires only when the"
  echo "# database's applied set is precisely these versions AND checksums."
  echo "# Anything else is left untouched and reported (crates/nook-db/src/restamp.rs)."
  echo "set $SET"
  echo "new $NEW_VERSION $NEW_CHECKSUM $(description_of "$NEW_NAME")"
  for f in "${FILES[@]}"; do
    echo "old $(version_of "$f") $(checksum_of "$f")"
  done
} > "$WORK/squash-manifest.txt"

if [[ $APPLY -eq 0 ]]; then
  say "dry run — verified but nothing written. Re-run with --apply to replace the set."
  echo "  would write: $MIG_DIR/$NEW_NAME  ($(wc -l < "$WORK/$NEW_NAME") lines)"
  echo "  would write: $MIG_DIR/squash-manifest.txt  (${#FILES[@]} old rows)"
  echo "  would delete: ${#FILES[@]} existing .sql files"
  exit 0
fi

# --- 7. Apply to the repo. ----------------------------------------------------
say "writing the squashed set"
for f in "${FILES[@]}"; do rm -f "$f"; done
cp "$WORK/$NEW_NAME" "$MIG_DIR/$NEW_NAME"
cp "$WORK/squash-manifest.txt" "$MIG_DIR/squash-manifest.txt"
echo "  $MIG_DIR/$NEW_NAME"
echo "  $MIG_DIR/squash-manifest.txt"

cat <<EOF

Next, and NOT optional — verification (c):
  1. touch crates/nook-control/src/lib.rs   # re-embed the migration set
  2. ./test.sh                              # a real binary boots the squashed set
An existing database re-stamps itself at boot; it is never re-migrated.
EOF
