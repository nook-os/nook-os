#!/usr/bin/env bash
# The shared operator node (MAIN-125). Identical join/run shape to the plain
# prod node (node-prod-entrypoint.sh) — join the control plane ONCE, then run —
# but this machine is the deployment's shared operator, not a person's own.
#
# Persistence is the operator's job via volumes: mount $HOME/.config/nook (node
# identity + generated SSH key), the workspace root, and the tmux runtime dir
# (TMUX_TMPDIR) on durable volumes and a restart re-attaches as the SAME node
# with its checkouts intact (AC-5). Note: a container stop terminates the tmux
# server process, so in-flight interactive sessions do not resume — the volume
# preserves the socket dir and the on-disk state, not the running processes.
# A FRESH volume has no node.toml, so it re-joins as a brand-new node rather than
# half-reusing state.
set -euo pipefail

export HOME=${HOME:-/root}
ROOT=${NOOK_WORKSPACE_ROOT:-/workspace}
mkdir -p "$ROOT"

# tmux runtime dir on its own volume (AC-2). tmux honours TMUX_TMPDIR for its
# socket; nook shells out to tmux and inherits this env.
export TMUX_TMPDIR=${TMUX_TMPDIR:-/var/lib/nook/tmux}
mkdir -p "$TMUX_TMPDIR"

# Mark this machine as the shared operator so it registers with the designation
# (capabilities.shared_operator) the Nodes UI and `nook get nodes` surface. The
# image already sets this; keep it here too for a bare `docker run` of the image.
export NOOK_SHARED_OPERATOR=${NOOK_SHARED_OPERATOR:-1}

# Join is operator-driven (a deploy-provisioned token), never self-service.
# Preferred: mount a TOML join config at /etc/nook/join.toml. Fallback: env.
if [ ! -f "$HOME/.config/nook/node.toml" ]; then
  if [ -f /etc/nook/join.toml ]; then
    nook join --config /etc/nook/join.toml --workspace-root "$ROOT"
  else
    : "${NOOK_SERVER:?set NOOK_SERVER (or mount /etc/nook/join.toml)}"
    : "${NOOK_JOIN_TOKEN:?set NOOK_JOIN_TOKEN (create one in the UI or POST /api/v1/nodes/join-tokens)}"
    nook join \
      --server "$NOOK_SERVER" \
      --token "$NOOK_JOIN_TOKEN" \
      --name "${NOOK_NODE_NAME:-operator-$(hostname)}" \
      --workspace-root "$ROOT"
  fi
fi

exec nook run
