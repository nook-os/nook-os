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

# A second stack on one machine needs its own compose project name, or the two
# checkouts fight over the same container names and volumes. Derive it from the
# directory so it is stable per worktree and needs no configuration.
if [ -z "${COMPOSE_PROJECT_NAME:-}" ]; then
  COMPOSE_PROJECT_NAME="nook-$(basename "$PWD" | tr -c '[:alnum:]_-' '-' | tr '[:upper:]' '[:lower:]')"
  export COMPOSE_PROJECT_NAME
fi
say "Compose project: $COMPOSE_PROJECT_NAME"

# Populate the cargo registry with ONE process before the three that share it
# start (MAIN-425). control-plane, chat and worker each mount `cargo-registry`
# and each run `cargo watch`, so on a COLD volume all three race to unpack the
# same crates and lose:
#
#   failed to open .../aws-sigv4-1.5.1/.cargo-ok — File exists (os error 17)
#
# and the loser exits 101 with no file change to make cargo-watch retry — a
# control plane that is "Up" and never serves. A primary checkout never sees it
# because its registry was populated long ago; a SECOND stack has an empty
# volume by definition, which is why this surfaced only once two stacks were
# possible. Prewarming is skipped once the volume exists, so it costs one
# fetch per project, ever.
if ! docker volume inspect "${COMPOSE_PROJECT_NAME}_cargo-registry" >/dev/null 2>&1; then
  say "Cold cargo registry — fetching dependencies once before the workers start"
  docker compose run --rm --no-deps --entrypoint "" control-plane cargo fetch \
    || say "  (fetch failed — continuing; the services will populate it themselves)"
fi

if [ "${1:-}" = "--build" ]; then
  say "Rebuilding images..."
  docker compose up -d --build
else
  docker compose up -d
fi

port="${NOOK_CONTROL_PORT:-8080}"
say "Waiting for the control plane on :$port ..."
for _ in $(seq 1 90); do
  if curl -fsS "http://localhost:$port/healthz" >/dev/null 2>&1; then
    say "Control plane healthy — web on http://localhost:${NOOK_WEB_PORT:-5173}"
    exit 0
  fi
  sleep 2
done

echo "✗ the control plane did not answer /healthz on :$port within 180s" >&2
echo "  docker compose logs control-plane | tail -50" >&2
exit 1
