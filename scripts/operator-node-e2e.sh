#!/usr/bin/env bash
# Integration test for the shared operator node (MAIN-125), driven by
# docker-compose. It brings up Postgres + the control plane, builds and starts
# the operator-node container, and asserts — with no API auth, straight from the
# nodes table — that the node:
#
#   1. joins and comes ONLINE,
#   2. reports the shared-operator designation (capabilities.shared_operator),
#   3. keeps the SAME identity across a container restart (AC-5).
#
# Assertions read Postgres directly (docker compose exec postgres psql), because
# the capabilities are stored as JSON on the nodes row and a WS-connected node
# sets status='online' — no user token required.
#
# Run:  scripts/operator-node-e2e.sh            (builds + up + assert + teardown)
#       scripts/operator-node-e2e.sh --keep     (leave the stack running)
#
# CI runs this on demand (workflow_dispatch); it is intentionally NOT on every
# PR — building the operator image pulls the agent runtimes from the network.
set -euo pipefail

cd "$(dirname "$0")/.."

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

# A well-known token for this run: the control plane seeds it (via the override)
# and the operator-node service consumes it. Not a secret — a throwaway dev DB.
export NOOK_DEV_JOIN_TOKEN="e2e-operator-join-token"

COMPOSE=(docker compose
  -f docker-compose.yml
  -f deploy/docker/operator-node-e2e.override.yml)

NODE_NAME="dev-operator"

cleanup() {
  if [ "$KEEP" -eq 1 ]; then
    echo "==> --keep: leaving the stack up (docker compose down -v to remove)"
    return
  fi
  echo "==> Tearing down"
  "${COMPOSE[@]}" --profile operator down -v >/dev/null 2>&1 || true
}
trap cleanup EXIT

# psql helper: run a query against the compose Postgres, print a trimmed scalar.
psql_scalar() {
  "${COMPOSE[@]}" exec -T postgres \
    psql -U nook -d nook -tAc "$1" 2>/dev/null | tr -d '[:space:]'
}

# Poll a query until it prints the wanted value, or time out.
wait_for() {
  local label="$1" query="$2" want="$3" timeout="$4" got
  echo "==> Waiting for: $label (<= ${timeout}s)"
  for _ in $(seq 1 "$timeout"); do
    got="$(psql_scalar "$query" || true)"
    if [ "$got" = "$want" ]; then
      echo "  ok: $label"
      return 0
    fi
    sleep 1
  done
  echo "  FAIL: $label — last value: '${got:-}' (wanted '$want')"
  return 1
}

echo "==> Bringing up Postgres + control plane"
"${COMPOSE[@]}" up -d postgres control-plane

echo "==> Waiting for the control plane to be healthy"
for _ in $(seq 1 120); do
  if "${COMPOSE[@]}" exec -T control-plane curl -fsS http://localhost:8080/healthz >/dev/null 2>&1; then
    break
  fi
  sleep 2
done

echo "==> Building and starting the operator node"
"${COMPOSE[@]}" --profile operator up -d --build operator-node

# 1 + 2: it joins, comes online, and is marked shared.
wait_for "operator node ONLINE" \
  "SELECT status FROM nodes WHERE name = '$NODE_NAME'" \
  "online" 90
wait_for "shared-operator designation" \
  "SELECT capabilities->>'shared_operator' FROM nodes WHERE name = '$NODE_NAME'" \
  "true" 60

# Capture identity before the restart.
NODE_ID_BEFORE="$(psql_scalar "SELECT id FROM nodes WHERE name = '$NODE_NAME'")"
echo "==> Node id before restart: $NODE_ID_BEFORE"
[ -n "$NODE_ID_BEFORE" ] || { echo "  FAIL: no node id"; exit 1; }

# 3: restart the container; identity persists on the config volume.
echo "==> Restarting the operator node"
"${COMPOSE[@]}" --profile operator restart operator-node
wait_for "operator node ONLINE again" \
  "SELECT status FROM nodes WHERE name = '$NODE_NAME'" \
  "online" 90

NODE_ID_AFTER="$(psql_scalar "SELECT id FROM nodes WHERE name = '$NODE_NAME'")"
echo "==> Node id after restart:  $NODE_ID_AFTER"
if [ "$NODE_ID_BEFORE" != "$NODE_ID_AFTER" ]; then
  echo "  FAIL: identity changed across restart ($NODE_ID_BEFORE -> $NODE_ID_AFTER)"
  exit 1
fi
echo "  ok: identity preserved across restart"

echo "operator-node integration test PASSED"
