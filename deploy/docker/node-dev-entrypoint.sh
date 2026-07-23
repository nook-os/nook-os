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

# The binary cargo just produced — NOT ./target/debug/nook.
#
# `CARGO_TARGET_DIR` points at a container-local volume, so `./target` is the
# HOST's build directory arriving over the bind mount: a binary from whenever
# somebody last ran cargo on their laptop, or missing entirely on a machine
# that never has. Joining and enrolling ran against that stale copy, which
# shows up as a feature mysteriously not working in dev while the tests pass.
NOOK="${CARGO_TARGET_DIR:-/app/target}/debug/nook"

if [ ! -f "$HOME/.config/nook/node.toml" ]; then
  "$NOOK" join \
    --server "${NOOK_SERVER:?}" \
    --token "${NOOK_DEV_JOIN_TOKEN:?}" \
    --name "${NOOK_NODE_NAME:-dev-node}" \
    --workspace-root /workspace
fi

# Enrol for mutual TLS, so the dev stack exercises the path production uses.
#
# Joining gets a token; enrolling gets a CERTIFICATE, and the two are not the
# same trust. Until this ran, the dev node authenticated with a bearer token and
# nothing here ever touched the mTLS listener, certificate renewal or CA
# rotation — the parts most worth having tested locally, and the ones a token
# node silently skips.
#
# The agent port serves a self-signed dev certificate, so the fingerprint is
# pinned from the cert the control plane is actually configured with rather
# than trusting the web PKI, which would (correctly) reject it.
if [ ! -f "$HOME/.config/nook/node.crt" ] && [ -f /app/deploy/dev-certs/agent.crt ]; then
  AGENT_FP=$(openssl x509 -in /app/deploy/dev-certs/agent.crt -noout -fingerprint -sha256 \
    | cut -d= -f2 | tr -d ':' | tr 'A-F' 'a-f')
  echo "▸ enrolling for mutual TLS (pinning ${AGENT_FP:0:16}…)"
  # Never fatal: a dev stack that cannot enrol should still come up on the
  # token path rather than refusing to start.
  "$NOOK" enroll \
    --server "${NOOK_AGENT_URL:-https://control-plane:8081}" \
    --token "${NOOK_DEV_JOIN_TOKEN:?}" \
    --server-fingerprint "$AGENT_FP" || echo "  (enrolment failed — continuing on the token path)"
fi

# --poll and the manifests, for the same reasons as the control plane: see
# the comment on its command in docker-compose.yml.
exec cargo watch --poll -w crates -w Cargo.toml -w Cargo.lock -x 'run -p nook-node -- run'
