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
