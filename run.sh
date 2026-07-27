#!/usr/bin/env bash
# Recreates the entire NookOS dev environment from scratch.
# `docker compose down -v` destroys everything; this script brings it all back.
set -euo pipefail
cd "$(dirname "$0")"

say() { printf '\033[33m▸ %s\033[0m\n' "$*"; }

say "Checking prerequisites..."
command -v docker >/dev/null || { echo "docker is required"; exit 1; }
docker compose version >/dev/null || { echo "docker compose v2 is required"; exit 1; }

if [ ! -f .env ]; then
  say "No .env found — creating from .env.example"
  cp .env.example .env
  echo "  Edit .env to point OIDC_* at your IdP, or leave AUTH_DEV_MODE=true for dev-login."
fi

say "Destroying previous environment (docker compose down -v)..."
docker compose down -v --remove-orphans

if command -v cargo >/dev/null && command -v pnpm >/dev/null; then
  say "Regenerating TypeScript types from Rust..."
  ./scripts/gen-types.sh || echo "  (type-gen failed — using committed generated types)"
else
  say "cargo/pnpm not found — skipping type-gen (committed generated types will be used)"
fi

# The operator node ships in the default stack now (MAIN-140); it joins with
# NOOK_DEV_JOIN_TOKEN. An .env predating that token would leave it unable to
# join, so warn loudly — but never stop the stack over it.
dev_join_token="$(grep -E '^NOOK_DEV_JOIN_TOKEN=' .env 2>/dev/null | tail -1 | cut -d= -f2-)"
if [ -z "$dev_join_token" ]; then
  printf '\033[31m▲ NOOK_DEV_JOIN_TOKEN is unset/empty in .env\033[0m\n' >&2
  echo "  The operator node will start but cannot join the control plane." >&2
  echo "  Set NOOK_DEV_JOIN_TOKEN in .env (see .env.example) and re-run, or" >&2
  echo "  run without it: docker compose up -d --scale operator-node=0" >&2
fi

say "Building and starting the stack..."
docker compose up --build -d

say "Waiting for control plane..."
for i in $(seq 1 120); do
  if curl -fsS http://localhost:8080/healthz >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS http://localhost:8080/healthz >/dev/null || { echo "control plane failed to become healthy"; docker compose logs control-plane | tail -50; exit 1; }

say "NookOS is up."
echo
echo "  Web UI:        http://localhost:5173"
echo "  API:           http://localhost:8080  (docs at /docs)"
echo "  MCP:           http://localhost:8080/mcp"
echo
echo "  Add this machine as a node:"
echo "    cargo run -p nook-node -- join --server http://localhost:8080 --token <token from UI>"
