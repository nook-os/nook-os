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
  # `tr -c` would convert the trailing newline too, leaving a stray hyphen
  # (MAIN-430 AC-5) — drop it before translating rather than after.
  COMPOSE_PROJECT_NAME="nook-$(basename "$PWD" | tr -d '\n' | tr -c '[:alnum:]_-' '-' | tr '[:upper:]' '[:lower:]')"
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
# control plane that is "Up" and never serves.
#
# The gate is a MARKER INSIDE the volume, not the volume's existence (MAIN-430).
# Existence cannot work: the `docker compose run` that fetches is itself what
# creates the volume, so a failed fetch left an existing-but-empty volume that
# the next run skipped — one transient failure permanently disarming the guard
# for that project, on exactly the cold-volume case it exists to protect. The
# marker is written only after a fetch that succeeded, so a failure simply
# leaves the next run to try again.
#
# One container start does the check and the work together: warm exits
# immediately, cold fetches and marks.
say "Checking the cargo registry"
if ! docker compose run --rm --no-deps --entrypoint "" control-plane sh -c '
      marker=/usr/local/cargo/registry/.nook-prewarmed
      if [ -f "$marker" ]; then exit 0; fi
      echo "▸ cold registry — fetching dependencies once before the workers start"
      cargo fetch && touch "$marker"
    '; then
  # Fatal, deliberately. Proceeding would start the three writers against a
  # registry that is cold precisely because the fetch failed — the silent race
  # above, which presents as a healthy container that never answers. A loud
  # stop here is recoverable; that is not.
  echo "✗ could not prewarm the cargo registry" >&2
  echo "  Starting now would run the three cargo-watch services against a cold" >&2
  echo "  shared registry, whose loser exits 101 and never retries." >&2
  echo "  Fix the cause (network, disk, registry auth) and re-run this script —" >&2
  echo "  it retries automatically, because nothing was marked as warmed." >&2
  exit 1
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
