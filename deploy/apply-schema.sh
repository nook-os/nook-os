#!/usr/bin/env bash
# Bring a RUNNING deployment's schema up to date, without destroying it.
#
# The bootstrap workflow edits 0001_init.sql in place and recreates the
# database. That's right for a dev loop and wrong for a deployment with real
# repos and real users in it, so this is the other half: apply the same
# changes as ALTERs, then re-stamp the migration checksum.
#
# The re-stamp matters. sqlx records a SHA-384 of each applied migration and
# refuses to start when the file has changed — which it always has, under the
# bootstrap workflow. Editing 0001 and deploying without this step takes the
# control plane down on boot.
#
# Usage (on the deployment host, from the repo root):
#   deploy/apply-schema.sh <postgres-container-name> [db-user] [db-name]
set -euo pipefail
cd "$(dirname "$0")/.."

CONTAINER="${1:?usage: apply-schema.sh <postgres-container> [user] [db]}"
DB_USER="${2:-nook}"
DB_NAME="${3:-nook}"
MIGRATION=crates/nook-control/migrations/0001_init.sql

say() { printf '\033[33m▸ %s\033[0m\n' "$*"; }

say "Applying schema deltas (idempotent)..."
docker exec -i "$CONTAINER" psql -v ON_ERROR_STOP=1 -U "$DB_USER" -d "$DB_NAME" \
  < deploy/prod-schema-delta.sql

# sqlx compares this against the file on every boot.
say "Re-stamping the 0001 migration checksum..."
CHECKSUM=$(sha384sum "$MIGRATION" | cut -d' ' -f1)
docker exec -i "$CONTAINER" psql -v ON_ERROR_STOP=1 -U "$DB_USER" -d "$DB_NAME" -c \
  "UPDATE _sqlx_migrations SET checksum = decode('$CHECKSUM', 'hex') WHERE version = 1;"

say "Schema is up to date. Safe to restart the control plane."
