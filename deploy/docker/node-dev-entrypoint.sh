#!/usr/bin/env bash
# Dev node container: seed demo repos, join once, then run under cargo watch.
set -euo pipefail
export HOME=/root

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
seed_repo workspace-1
seed_repo workspace-2
seed_repo workspace-3

# Build once so `nook join` exists, join if this container has no identity
# yet, then let cargo watch own the run loop (rebuild + restart on change).
cargo build -p nook-node

if [ ! -f "$HOME/.config/nook/node.toml" ]; then
  ./target/debug/nook join \
    --server "${NOOK_SERVER:?}" \
    --token "${NOOK_DEV_JOIN_TOKEN:?}" \
    --name "${NOOK_NODE_NAME:-dev-node}" \
    --workspace-root /workspace
fi

# --poll and the manifests, for the same reasons as the control plane: see
# the comment on its command in docker-compose.yml.
exec cargo watch --poll -w crates -w Cargo.toml -w Cargo.lock -x 'run -p nook-node -- run'
