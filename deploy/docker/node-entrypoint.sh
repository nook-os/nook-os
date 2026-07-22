#!/usr/bin/env bash
# Dev node container: seed demo workspaces, auto-join the control plane, run the agent.
set -euo pipefail

export HOME=/root

# Demo git repositories so workspace discovery has something to find.
seed_repo() {
  local name=$1
  local dir=/workspace/$name
  if [ ! -d "$dir/.git" ]; then
    mkdir -p "$dir"
    git -C "$dir" init -q -b main
    git -C "$dir" config user.email dev@nookos.local
    git -C "$dir" config user.name "NookOS Dev"
    echo "# $name" > "$dir/README.md"
    git -C "$dir" add . && git -C "$dir" commit -qm "initial commit"
    git -C "$dir" remote add origin "https://github.com/nookos-demo/$name.git"
  fi
}
seed_repo globex
seed_repo acme
seed_repo widgets

if [ ! -f "$HOME/.config/nook/node.toml" ]; then
  nook join \
    --server "${NOOK_SERVER:?}" \
    --token "${NOOK_DEV_JOIN_TOKEN:?}" \
    --name "${NOOK_NODE_NAME:-node-1}" \
    --workspace-root /workspace
fi

exec nook run
