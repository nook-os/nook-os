#!/usr/bin/env bash
# Bring the stack up from ANY checkout, including a freshly created worktree
# with nothing hand-copied (MAIN-425 AC-1).
#
#   ./scripts/dev-up.sh            start (build if the images are missing)
#   ./scripts/dev-up.sh --build    rebuild the images first
#
# The difference from `run.sh`: this one only BOOTS. `run.sh` destroys the
# environment (`down -v`) and reseeds the dogfood world — the right thing for
# resetting your primary checkout, and the wrong thing for a second stack you
# want alongside it, because it would take the shared volumes with it.
#
# Ports come from the environment, which is what makes two stacks possible: a
# nook session leases eleven and exports them (`.nook.toml`), and every host
# port in docker-compose.yml is `${VAR:-default}`. Outside a session the
# defaults apply and this is the ordinary single-stack path.
set -euo pipefail

cd "$(dirname "$0")/.."

say() { printf '\033[36m▸ %s\033[0m\n' "$1"; }

./scripts/dev-bootstrap.sh

# shellcheck source=scripts/compose-project.sh
. ./scripts/compose-project.sh
say "Compose project: $COMPOSE_PROJECT_NAME"

# The cold-registry guard (MAIN-425/430) — shared with run.sh so the two
# cannot drift; the why lives in the script.
say "Checking the cargo registry"
./scripts/dev-prewarm.sh

if [ "${1:-}" = "--build" ]; then
  say "Rebuilding images..."
  docker compose up -d --build
else
  docker compose up -d
fi

# Compile-aware, shared with run.sh: a cold cache builds for many minutes
# before the first /healthz, and the flat 180s timer here called that a
# failed boot.
port="${NOOK_CONTROL_PORT:-8080}"
say "Waiting for the control plane on :$port (a cold cache compiles first)..."
./scripts/dev-wait-healthy.sh "http://localhost:$port"
say "Control plane healthy — web on http://localhost:${NOOK_WEB_PORT:-5173}"
