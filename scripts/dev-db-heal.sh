#!/usr/bin/env bash
# Heal a DEV migration ledger that is ahead of the checked-out migration set
# (MAIN-224).
#
# The accident this recovers from: a branch carrying migration N ran against the
# shared dev database, recording N in `_sqlx_migrations`. Every checkout without
# that `.sql` file now has an orphan ledger row. The control plane's boot already
# WARNs past those rows in dev (`run_with_dev_tolerance`) instead of dying; this
# script is the deliberate cleanup that removes them so the ledger matches the
# tree again.
#
# Usage:
#   scripts/dev-db-heal.sh                 list orphan ledger rows (dry run)
#   scripts/dev-db-heal.sh --fix           delete them (asks first)
#   scripts/dev-db-heal.sh --fix --yes     delete them without the prompt
#   scripts/dev-db-heal.sh --chat [...]    target chat's ledger (chat._sqlx_migrations)
#
# It also DETECTS the other ledger failure — a version applied from a different
# file than the tree now holds ("previously applied but has been modified") —
# and refuses to touch it, because the repair there is to recreate the dev
# volume, not to rewrite the ledger (MAIN-425 AC-5).
#
# Safety (AC-3): it refuses when APP_ENV=production and refuses any DATABASE_URL
# whose host is not local or a compose service name. The heuristic is deliberately
# strict — it would rather refuse a legitimate but unusual dev URL than ever touch
# a production database. It reads DATABASE_URL and needs `psql` on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."

FIX=0
CHAT=0
ASSUME_YES=0
for arg in "$@"; do
  case "$arg" in
    --fix) FIX=1 ;;
    --chat) CHAT=1 ;;
    --yes | -y) ASSUME_YES=1 ;;
    -h | --help)
      sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown argument: $arg (try --help)" >&2
      exit 2
      ;;
  esac
done

# --- Guard 1: never in production. ---------------------------------------------
if [[ "${APP_ENV:-}" == "production" ]]; then
  echo "refusing: APP_ENV=production — this script is a dev-only tool and never" >&2
  echo "touches a production ledger." >&2
  exit 1
fi

# --- Guard 2: DATABASE_URL must be present and point at a local/compose host. ---
if [[ -z "${DATABASE_URL:-}" ]]; then
  echo "refusing: DATABASE_URL is not set." >&2
  exit 1
fi

# Pull the host out of postgres://[user[:pass]@]host[:port]/db, handling a
# bracketed IPv6 literal ([::1]).
authority="${DATABASE_URL#*://}" # user:pass@host:port/db
authority="${authority%%/*}"     # user:pass@host:port
hostport="${authority##*@}"      # host:port  (strips any credentials)
if [[ "$hostport" == "["*"]"* ]]; then
  host="${hostport#\[}"
  host="${host%%\]*}" # ::1
else
  host="${hostport%%:*}" # host
fi
host="$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')"

case "$host" in
  localhost | 127.0.0.1 | ::1 | postgres | db) ;;
  *)
    echo "refusing: DATABASE_URL host '$host' is not local or a compose service" >&2
    echo "name (allowed: localhost, 127.0.0.1, ::1, postgres, db). Better to refuse" >&2
    echo "a legitimate dev URL than to risk touching production." >&2
    exit 1
    ;;
esac

# --- Guard 3: psql is the DB client this script drives. ------------------------
if ! command -v psql >/dev/null 2>&1; then
  echo "refusing: psql not found on PATH. Install postgresql-client, or run this" >&2
  echo "inside the compose Postgres container (docker compose exec postgres ...)." >&2
  exit 1
fi

if [[ "$CHAT" -eq 1 ]]; then
  LEDGER="chat._sqlx_migrations"
  MIG_DIR="crates/nook-chat/migrations"
else
  LEDGER="public._sqlx_migrations"
  MIG_DIR="crates/nook-control/migrations"
fi

# Local versions: the numeric filename prefix of each up-migration .sql, base-10
# (10# avoids an octal reading of a zero-padded prefix like 0008).
declare -A LOCAL
for f in "$MIG_DIR"/*.sql; do
  base="$(basename "$f")"
  case "$base" in
    *.down.sql) continue ;;
  esac
  LOCAL["$((10#${base%%_*}))"]=1
done

# Ledger is empty (or absent) → nothing applied, nothing to heal.
if [[ "$(psql "$DATABASE_URL" -tAc "SELECT to_regclass('$LEDGER')")" == "" ]]; then
  echo "ledger $LEDGER does not exist yet — nothing to heal."
  exit 0
fi

mapfile -t applied < <(psql "$DATABASE_URL" -tAc "SELECT version FROM $LEDGER ORDER BY version")

# ── modified migrations (MAIN-425 AC-5) ─────────────────────────────────────
# A DIFFERENT failure from the orphan rows above, and one this script must
# report rather than fix. A reused worktree directory keeps its pgdata volume
# across branch switches, so version N can have been applied from an abandoned
# branch whose N_*.sql differed. Boot then dies with "migration N was previously
# applied but has been modified" — fatal in dev too, because
# `run_with_dev_tolerance` tolerates a MISSING version and never a changed one.
#
# Deleting the row would be the wrong repair twice over: the migrator would
# re-apply N against a schema that already has it, and the checksum is the only
# proof that the schema in front of you is the schema the repo describes
# (CLAUDE.md). So this reports and stops. The answer is to recreate the volume.
#
# sqlx checksums a migration with SHA-384 over the file's bytes, which is what
# makes this comparable without running the migrator.
modified=()
if command -v sha384sum >/dev/null; then
  while IFS='|' read -r v stored; do
    [[ -z "$v" ]] && continue
    # Glob rather than `ls | grep`: the version's up-migration, down excluded.
    f=""
    for cand in "$MIG_DIR"/"$(printf '%04d' "$v")"_*.sql; do
      case "$cand" in
        *.down.sql) continue ;;
      esac
      [[ -f "$cand" ]] && { f="$cand"; break; }
    done
    [[ -z "$f" ]] && continue   # missing file is the orphan case, handled below
    actual=$(sha384sum "$f" | cut -d' ' -f1)
    [[ "$stored" != "$actual" ]] && modified+=("$v|$(basename "$f")")
  done < <(psql "$DATABASE_URL" -tAc \
    "SELECT version, encode(checksum, 'hex') FROM $LEDGER ORDER BY version")
fi

if [[ "${#modified[@]}" -gt 0 ]]; then
  echo "✗ $LEDGER: ${#modified[@]} migration(s) applied from a DIFFERENT file than the tree holds:" >&2
  for m in "${modified[@]}"; do
    echo "    version ${m%%|*}  (${m##*|})" >&2
  done
  cat >&2 <<'MSG'

  This is NOT the orphan case and --fix will not touch it. The ledger row is
  correct about what ran; the tree has since changed underneath it. Deleting the
  row would re-apply the migration onto a schema that already has it, and the
  checksum is the only proof the schema matches the repo.

  The dev answer is to recreate the database, which is cheap because a dev
  volume holds nothing you need:

      docker compose down -v && ./run.sh

  For a SECOND stack, take only its own volumes with it:

      COMPOSE_PROJECT_NAME=<that stack's project> docker compose down -v

  If the modified file is one YOU changed, restore it instead — an applied
  migration is append-only; the fix is a NEW numbered file.
MSG
  exit 1
fi

orphans=()
for v in "${applied[@]}"; do
  [[ -z "$v" ]] && continue
  if [[ -z "${LOCAL[$v]:-}" ]]; then
    orphans+=("$v")
  fi
done

if [[ "${#orphans[@]}" -eq 0 ]]; then
  echo "$LEDGER: no orphan rows — the ledger matches $MIG_DIR."
  exit 0
fi

echo "$LEDGER: ${#orphans[@]} orphan row(s) with no matching file in $MIG_DIR:"
for v in "${orphans[@]}"; do
  echo "  version $v"
done

if [[ "$FIX" -ne 1 ]]; then
  echo "(dry run — re-run with --fix to delete exactly these rows.)"
  exit 0
fi

if [[ "$ASSUME_YES" -ne 1 ]]; then
  read -r -p "Delete these ${#orphans[@]} row(s) from $LEDGER? [y/N] " reply
  case "$reply" in
    y | Y | yes | YES) ;;
    *)
      echo "aborted; nothing deleted."
      exit 0
      ;;
  esac
fi

# Comma-join the orphan versions (all integers, parsed above) for one DELETE.
list="$(
  IFS=,
  echo "${orphans[*]}"
)"
psql "$DATABASE_URL" -qc "DELETE FROM $LEDGER WHERE version IN ($list)"
echo "deleted ${#orphans[@]} orphan row(s) from $LEDGER."
