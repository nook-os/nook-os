#!/usr/bin/env bash
# Production node container: join the control plane once (config persists in a
# volume), then run the agent. No demo seeding — real workspaces only.
set -euo pipefail

export HOME=${HOME:-/root}
ROOT=${NOOK_WORKSPACE_ROOT:-/workspace}
mkdir -p "$ROOT"

if [ ! -f "$HOME/.config/nook/node.toml" ]; then
  : "${NOOK_SERVER:?set NOOK_SERVER to the control plane URL}"
  : "${NOOK_JOIN_TOKEN:?set NOOK_JOIN_TOKEN (create one in the UI or POST /api/v1/nodes/join-tokens)}"
  nook join \
    --server "$NOOK_SERVER" \
    --token "$NOOK_JOIN_TOKEN" \
    --name "${NOOK_NODE_NAME:-$(hostname)}" \
    --workspace-root "$ROOT"
fi

exec nook run
