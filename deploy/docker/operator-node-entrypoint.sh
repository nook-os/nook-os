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
#
# A missing token must NOT crash the machine that ships in every dev stack now
# (MAIN-140): if there is no existing identity, no join.toml, and no token, say
# LOUDLY and exactly why the operator can't join, then stop — the rest of the
# stack is unaffected, and `docker compose logs operator-node` shows the reason.
if [ ! -f "$HOME/.config/nook/node.toml" ]; then
  if [ -f /etc/nook/join.toml ]; then
    nook join --config /etc/nook/join.toml --workspace-root "$ROOT"
  elif [ -n "${NOOK_JOIN_TOKEN:-}" ] && [ -n "${NOOK_SERVER:-}" ]; then
    nook join \
      --server "$NOOK_SERVER" \
      --token "$NOOK_JOIN_TOKEN" \
      --name "${NOOK_NODE_NAME:-operator-$(hostname)}" \
      --workspace-root "$ROOT"
  else
    echo "═══════════════════════════════════════════════════════════════════" >&2
    echo "operator node: cannot join — NOT joining, container will stop." >&2
    echo >&2
    if [ -z "${NOOK_JOIN_TOKEN:-}" ]; then
      echo "  NOOK_JOIN_TOKEN is empty. In the dev stack it comes from" >&2
      echo "  NOOK_DEV_JOIN_TOKEN in your .env — set it (see .env.example) and" >&2
      echo "  re-run ./run.sh, or mint a token in the UI / POST" >&2
      echo "  /api/v1/nodes/join-tokens." >&2
    fi
    [ -z "${NOOK_SERVER:-}" ] && echo "  NOOK_SERVER is unset." >&2
    echo >&2
    echo "  The rest of the stack is running normally without the operator." >&2
    echo "═══════════════════════════════════════════════════════════════════" >&2
    exit 0
  fi
fi

exec nook run
