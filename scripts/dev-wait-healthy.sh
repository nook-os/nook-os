#!/usr/bin/env bash
# Wait for this checkout's control plane to answer /healthz — for as long as it
# is visibly making progress.
#
#   ./scripts/dev-wait-healthy.sh http://localhost:8080
#
# The flat 120s/180s timers this replaces could not tell "compiling" from
# "dead": cargo-watch builds INSIDE the container, and a cold `.cache` means
# the first build takes many minutes — so a boot that was going to succeed got
# reported as "failed to become healthy", and the operator went debugging a
# stack that only needed patience. The distinction that matters:
#
#   healthy                       → done
#   container gone                → dead, dump the logs
#   container up, logs moving     → building; wait and show the last line
#   container up, logs SILENT     → wedged; give up after $NOOK_WAIT_IDLE
#
# A hard cap ($NOOK_WAIT_MAX) bounds the pathological case of a service that
# logs forever without ever serving — e.g. a crash/retry loop.
set -euo pipefail

cd "$(dirname "$0")/.."

# shellcheck source=scripts/compose-project.sh
. ./scripts/compose-project.sh

url="${1:?usage: dev-wait-healthy.sh <control plane base url>}"
idle_limit="${NOOK_WAIT_IDLE:-150}"
hard_limit="${NOOK_WAIT_MAX:-1800}"

fail_with_logs() {
  echo "✗ $1" >&2
  docker compose logs --tail 50 control-plane >&2 || true
  # The cold-registry race (MAIN-425): the loser exits 101 and cargo-watch
  # never retries, so waiting longer cannot help — name the actual fix.
  if docker compose logs --tail 200 control-plane 2>/dev/null \
      | grep -q 'File exists (os error 17)'; then
    echo "  This is the cold-registry race: run ./scripts/dev-prewarm.sh," >&2
    echo "  then restart the stack." >&2
  fi
  exit 1
}

start=$SECONDS
last_activity=$SECONDS
last_note=0
while :; do
  if curl -fsS "$url/healthz" >/dev/null 2>&1; then
    exit 0
  fi
  if ! docker compose ps --status running --services 2>/dev/null \
      | grep -qx control-plane; then
    fail_with_logs "the control-plane container is not running"
  fi

  # `|| true`: a transient compose failure here must read as "no activity",
  # not kill the wait under `set -e`.
  recent="$(docker compose logs --since 30s --no-log-prefix control-plane 2>/dev/null | tail -1 || true)"
  [ -n "$recent" ] && last_activity=$SECONDS

  elapsed=$((SECONDS - start))
  if [ $((SECONDS - last_activity)) -ge "$idle_limit" ]; then
    fail_with_logs "no /healthz and no log output for ${idle_limit}s — the control plane looks wedged"
  fi
  if [ "$elapsed" -ge "$hard_limit" ]; then
    fail_with_logs "still not healthy after ${hard_limit}s — giving up"
  fi

  if [ $((elapsed - last_note)) -ge 15 ]; then
    last_note=$elapsed
    printf '  … waiting (%dm%02ds)  %s\n' $((elapsed / 60)) $((elapsed % 60)) "$recent"
  fi
  sleep 2
done
