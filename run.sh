#!/usr/bin/env bash
# Recreates the entire NookOS dev environment from scratch.
# `docker compose down -v` destroys everything; this script brings it all back.
#
#   ./run.sh                  full recreate
#   ./run.sh --claude-login   only (re-)run the fleet's Claude device login
set -euo pipefail
cd "$(dirname "$0")"

say() { printf '\033[33m▸ %s\033[0m\n' "$*"; }
warn() { printf '\033[31m▲ %s\033[0m\n' "$*" >&2; }

# ── The fleet's Claude identity (MAIN-238) ───────────────────────────────────
#
# Loop jobs run `claude` on a node, and the executor query only places work on a
# node that reports the runtime AUTHORIZED — so without a login a spec job sits
# queued forever with "no eligible executor". The session lives in a gitignored
# mount (CLAUDE_CONFIG_DIR, honoured by claude 2.1.220) rather than the host's
# ~/.claude, which is what makes the fleet's identity separate from yours and
# swappable without touching your own login.
#
# Subscription device-login only. There is no API-key path here and there will
# not be one.
CLAUDE_DIR=".nook-secrets/claude"

# The container the login runs in. The operator node is the shared loop machine,
# so its identity is the fleet's.
CLAUDE_SVC="operator-node"

# `claude auth status` prints JSON (`--json` is its default), so this is a fact
# from the CLI rather than a guess from the filesystem: a directory with files
# in it can still hold an expired session.
#   0 = logged in · 1 = not logged in · 2 = could not tell (not running, etc.)
claude_login_state() {
  local out
  if ! out="$(docker compose exec -T "$CLAUDE_SVC" sh -lc 'timeout 20 claude auth status --json' 2>/dev/null)"; then
    return 2
  fi
  case "$out" in
    *'"loggedIn": true'*|*'"loggedIn":true'*) return 0 ;;
    *'"loggedIn": false'*|*'"loggedIn":false'*) return 1 ;;
    *) return 2 ;;
  esac
}

# Run the device-authorization flow interactively. `claude auth login
# --claudeai` prints a verification URL and a code, then waits — so this needs a
# TTY and the operator's browser; it cannot be automated, by design.
claude_device_login() {
  say "Starting the Claude device login for the fleet ($CLAUDE_SVC)..."
  echo "  A verification URL and code will appear below. Complete it in a"
  echo "  browser, and this will continue once the session lands."
  echo
  if docker compose exec "$CLAUDE_SVC" sh -lc 'claude auth login --claudeai'; then
    if claude_login_state; then
      say "Logged in. The node will report the claude runtime authorized on its next heartbeat."
      return 0
    fi
    warn "The login command finished but the session still reads signed-out."
    echo "  Try again with: ./run.sh --claude-login" >&2
    return 1
  fi
  warn "The Claude login did not complete."
  echo "  The stack keeps running; loops just have no agent until you run:" >&2
  echo "    ./run.sh --claude-login" >&2
  return 1
}

# Boot-time detect-and-offer. Idempotent: a valid session is detected and
# nothing is asked. Declining is fine and says how to do it later.
claude_login_gate() {
  mkdir -p "$CLAUDE_DIR"
  # Capture the status explicitly. After an `if` whose condition fails and which
  # has no else branch, bash sets $? to 0 — so reading $? after `fi` would
  # collapse "not logged in" and "could not tell" into "fine", which is the one
  # thing this gate must never do. `|| rc=$?` also keeps `set -e` out of it.
  local rc=0
  claude_login_state || rc=$?
  if [ "$rc" = "0" ]; then
    say "Claude session present — loops have an agent."
    return 0
  fi
  if [ "$rc" = "2" ]; then
    warn "Could not read the Claude auth status from $CLAUDE_SVC."
    echo "  Loops may have no agent. Check with:" >&2
    echo "    docker compose exec $CLAUDE_SVC claude auth status" >&2
    return 0
  fi

  echo
  echo "  The fleet has no Claude session, so spec/decompose jobs will sit queued"
  echo "  with \"no eligible executor\" — nothing can run the agent."
  echo "  This is a device login for a SEPARATE identity, stored in $CLAUDE_DIR;"
  echo "  your own ~/.claude is not touched."
  local reply=""
  if [ -t 0 ]; then
    read -r -p "  Log in with Claude for specs/loops? [y/N] " reply
  else
    echo "  (non-interactive shell — skipping the prompt)"
  fi
  case "$reply" in
    [yY]|[yY][eE][sS]) claude_device_login || true ;;
    *) echo "  Skipped. Log in any time with: ./run.sh --claude-login" ;;
  esac
}

# `--claude-login` is the on-demand switch: it re-runs the flow against the same
# mount whether or not a session is already there, so swapping accounts is
# `rm -rf .nook-secrets/claude` (or `claude auth logout`) then this.
if [ "${1:-}" = "--claude-login" ]; then
  mkdir -p "$CLAUDE_DIR"
  if ! docker compose ps --status running --services 2>/dev/null | grep -qx "$CLAUDE_SVC"; then
    warn "$CLAUDE_SVC is not running. Start the stack first: docker compose up -d"
    exit 1
  fi
  claude_device_login
  exit $?
fi

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
for _ in $(seq 1 120); do
  if curl -fsS http://localhost:8080/healthz >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS http://localhost:8080/healthz >/dev/null || { echo "control plane failed to become healthy"; docker compose logs control-plane | tail -50; exit 1; }

# The agent the loop runs on — offered here, after the stack is healthy, so the
# operator-node is actually up to run the flow in.
claude_login_gate

say "NookOS is up."
echo
echo "  Web UI:        http://localhost:5173"
echo "  API:           http://localhost:8080  (docs at /docs)"
echo "  MCP:           http://localhost:8080/mcp"
echo
echo "  Fleet Claude login (specs/loops):"
echo "    ./run.sh --claude-login          # log in, or switch accounts"
echo "    docker compose exec operator-node claude auth status"
echo
echo "  Add this machine as a node:"
echo "    cargo run -p nook-node -- join --server http://localhost:8080 --token <token from UI>"
