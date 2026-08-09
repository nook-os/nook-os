#!/usr/bin/env bash
# Recreates the entire NookOS dev environment from scratch.
# `docker compose down -v` destroys the DATA volumes; this script brings it all
# back. Build caches live in ./.cache as bind mounts and deliberately survive.
#
#   ./run.sh                  full recreate, reusing the images already built
#   ./run.sh --build          the same, but rebuild the images first
#   ./run.sh --claude-login   only (re-)run the fleet's Claude device login
#
# IMAGES ARE NOT REBUILT BY DEFAULT, because for the Rust services there is
# nothing in them to rebuild: `dev-rust.Dockerfile` bakes a toolchain and no
# source, the repo is bind-mounted at /app, and cargo-watch compiles inside the
# running container. `--build` on every run re-sent the whole build context to
# re-derive an image that had not changed.
#
# TWO IMAGES ARE EXCEPTIONS, because they bake something.
#
# THE OPERATOR NODE is the reason `--build` still exists:
# `operator-node.Dockerfile` COPYs `crates/` and bakes a release binary, with no
# mount and no cargo-watch. It therefore keeps running whatever `nook` it was
# built with until somebody rebuilds it — and a stale one skews against the
# control plane's protocol and hangs loop jobs at "waiting for the agent", which
# looks like a hung job rather than an old image. After changing anything under
# `crates/`, run `./run.sh --build` (or `docker compose build operator-node`)
# before trusting a loop run.
#
# THE DEV NODE bakes an agent TOOLCHAIN (MAIN-486): `dev-rust.Dockerfile`'s
# `dev-node` stage carries `gh` and a pinned `claude`. That one is handled for
# you — this script checks the existing image for those binaries and rebuilds it
# when they are missing — because the failure it prevents is silent and remote
# from its cause: compose only builds an image that is ABSENT, never one whose
# Dockerfile changed, so a machine that already had a `node` image would keep a
# toolchain-less one and the symptom would surface much later as a build pass
# dying on `gh: not found`.
#
# A clean run lands in a state the loop can actually be tested in (MAIN-341):
# a real bare git repo on the operator node, a seeded workspace pointing at it,
# loops on, and a ticket ready to draft a spec against. The ONE manual step is
# the Claude device login below.
set -euo pipefail

# Where THIS checkout's control plane and web app actually are (MAIN-376).
#
# These used to be safe literals, because every checkout published 8080 and
# 5173. They are not any more: a session leases its own ports, so a hardcoded
# `localhost:8080` here either fails the health gate against a stack that is
# perfectly healthy, or — worse, on a machine running a second checkout —
# SUCCEEDS against that other stack and writes `contexts.toml` pointing at it,
# with a token minted there. MAIN-341's promise is that `contexts.toml` is valid
# after every reseed; addressing somebody else's control plane breaks it
# silently.
#
# Same defaults as `docker-compose.yml`, so a plain clone is unchanged (AC-3).
CONTROL_URL="http://localhost:${NOOK_CONTROL_PORT:-8080}"
WEB_URL="http://localhost:${NOOK_WEB_PORT:-5173}"
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

# A login is not enough to make the session USABLE BY AN AGENT.
#
# `claude auth login` writes the credential and the account, but not
# `hasCompletedOnboarding`. The next interactive launch therefore opens on the
# "Select login method" picker — and a loop run has nobody to press a key. That
# is not a hypothetical: it stopped every managed review session on this stack,
# one every five minutes overnight, while `claude auth status` cheerfully
# reported `loggedIn: true` the whole time.
#
# Reviews are headless runs now and no longer go through that prompt, but a
# person's `claude` session on a node still does, and it costs one line to make
# the login mean what everybody already assumes it means.
claude_mark_onboarded() {
  docker compose exec -T "$CLAUDE_SVC" sh -lc '
    f="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.claude.json"
    [ -f "$f" ] || exit 0
    node -e "
      const fs=require(\"fs\"), p=process.argv[1];
      const d=JSON.parse(fs.readFileSync(p,\"utf8\"));
      if (d.hasCompletedOnboarding) process.exit(0);
      d.hasCompletedOnboarding = true;
      fs.writeFileSync(p, JSON.stringify(d, null, 2));
    " "$f"' 2>/dev/null || true
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
    claude_mark_onboarded
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
# Rebuild images only when asked. See the header for why the default is off and
# when it is wrong.
BUILD=""
for arg in "$@"; do
  case "$arg" in
    --build|--rebuild) BUILD="--build" ;;
  esac
done

if [ "${1:-}" = "--claude-login" ]; then
  mkdir -p "$CLAUDE_DIR"
  if ! docker compose ps --status running --services 2>/dev/null | grep -qx "$CLAUDE_SVC"; then
    warn "$CLAUDE_SVC is not running. Start the stack first: docker compose up -d"
    exit 1
  fi
  claude_device_login
  exit $?
fi

# ── The dogfood repo (MAIN-341) ──────────────────────────────────────────────
#
# A REAL bare git repo on the operator node's own disk, so a loop job clones it
# with no ssh key, no credential and no network. The seed points the
# `nook-dogfood` workspace at exactly this path — the two must agree, and the
# path is defined in `crates/nook-control/src/seed.rs` as `DOGFOOD_REPO_PATH`.
#
# Idempotent: a repo that is already there is left alone, so re-running this
# never discards commits somebody made while testing.
DOGFOOD_REPO="/workspace/nook-dogfood.git"

provision_dogfood_repo() {
  if ! docker compose ps --status running --services 2>/dev/null | grep -qx "$CLAUDE_SVC"; then
    warn "$CLAUDE_SVC is not running — skipping the dogfood repo."
    echo "  The seeded workspace will have nothing to clone until it is." >&2
    return 0
  fi
  # Piped to the container's shell through a QUOTED heredoc: nothing in here is
  # expanded by this shell, so the script the container runs is exactly the
  # script written below — no escaping, no second version to review.
  if docker compose exec -T "$CLAUDE_SVC" sh -s "$DOGFOOD_REPO" <<'PROVISION'
set -eu
REPO="$1"
[ -d "$REPO" ] && { echo "dogfood repo already present at $REPO"; exit 0; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
git init -q -b main "$tmp"
cd "$tmp"
# Committing needs an identity and the container has none. Local to this
# throwaway worktree, so it leaks nowhere.
git config user.email "dev@nookos.local"
git config user.name "NookOS dev"

cat > README.md <<'DOC'
# nook-dogfood

A tiny, real repository living on the NookOS operator node, so the agent loop can
be exercised end to end without ssh keys, credentials, or a network.

Deliberately small, deliberately not empty: `/nook-spec` researches a repository
before it drafts anything, and there has to be something to read.
DOC

cat > greet.py <<'DOC'
"""The one command this project has, so a spec has something to extend."""


def greet(name: str) -> str:
    return f"Hello, {name}!"


if __name__ == "__main__":
    print(greet("world"))
DOC

git add -A
git commit -qm "Initial commit: a greeting and a README"

# Bare, because a clone URL should point at one: cloning a non-bare checkout
# works, but pushing back to its checked-out branch is refused — and the loop
# pushes.
#
# `-b main` is load-bearing. Without it the bare repo's HEAD follows
# `init.defaultBranch` (`master` on a stock container) while the push lands on
# `main`, and a clone then checks out NOTHING from a repo that has a commit in
# it. Verified against the real operator node, both ways.
git init -q --bare -b main "$REPO"
git remote add origin "$REPO"
git push -q origin main
echo "dogfood repo created at $REPO"
PROVISION
  then
    say "Dogfood repo ready at $DOGFOOD_REPO (on $CLAUDE_SVC)."
  else
    warn "Could not provision the dogfood repo — the seeded workspace has nothing to clone."
  fi
}

# ── The dev CLI context (MAIN-341) ───────────────────────────────────────────
#
# `docker compose down -v` drops the database, so any token in contexts.toml is
# a 401 on the next boot and `nook` cannot drive dev until somebody notices.
#
# This mints a fresh one through a LOGIN — the dev-login hatch, which is gated
# on AUTH_DEV_MODE and refused outright in production — and hands it to
# `nook login`. Nothing is baked: no token is committed, and the value exists
# only in the response and in your own contexts.toml.
DEV_EMAIL="dev@nookos.local"

refresh_dev_cli_context() {
  command -v nook >/dev/null 2>&1 || return 0

  local jar token
  jar="$(mktemp)"
  # Sign in as the seeded dev identity. The seed puts that user in the same
  # tenant the operator node joins, which is what makes a placed job find an
  # executor (AC-4).
  if ! curl -fsS -c "$jar" -X POST "$CONTROL_URL"/api/v1/auth/dev-login \
      -H 'Content-Type: application/json' \
      -d "{\"email\":\"$DEV_EMAIL\"}" >/dev/null 2>&1; then
    rm -f "$jar"
    echo "  (dev-login unavailable — leaving the nook CLI context alone)"
    return 0
  fi
  token="$(curl -fsS -b "$jar" -X POST "$CONTROL_URL"/api/v1/tokens \
      -H 'Content-Type: application/json' -d '{"name":"dev cli"}' 2>/dev/null \
    | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')"
  rm -f "$jar"

  if [ -z "$token" ]; then
    warn "Could not mint a dev CLI token — \`nook\` may 401 against dev."
    return 0
  fi
  if nook login --token "$token" --server "$CONTROL_URL" >/dev/null 2>&1; then
    say "nook CLI signed in to dev as $DEV_EMAIL."
  else
    warn "Minted a dev token but \`nook login\` refused it."
  fi
}

# ── The dev build node's placement label (MAIN-486) ──────────────────────────
#
# Build work is placed only on a node labelled `role=build` (MAIN-383), and a
# label is a person's statement about a machine — nothing in the join path can
# set it, by design. So `run.sh` applies it to `dev-node` the way a human would:
# through the API, as the seeded dev identity that owns the node.
#
# The node joins moments after the control plane answers, so this retries
# briefly rather than racing it. If it still cannot find the node it prints the
# exact command instead of failing the boot — the label is the last step, and a
# stack that is otherwise up should not be torn down over it.
label_dev_build_node() {
  local jar id attempt
  jar="$(mktemp)"
  if ! curl -fsS -c "$jar" -X POST "$CONTROL_URL"/api/v1/auth/dev-login \
      -H 'Content-Type: application/json' \
      -d "{\"email\":\"$DEV_EMAIL\"}" >/dev/null 2>&1; then
    rm -f "$jar"
    warn "dev-login unavailable — dev-node is unlabelled and build jobs will stay queued."
    return 0
  fi
  id=""
  for attempt in 1 2 3 4 5 6 7 8 9 10; do
    # `id` is the first field serde writes for a node and `name` the third,
    # with only a scalar between — so one match pairs the two without a JSON
    # parser this script does not otherwise need.
    id="$(curl -fsS -b "$jar" "$CONTROL_URL"/api/v1/nodes 2>/dev/null \
      | grep -o '"id":"[^"]*","tenant_id":"[^"]*","name":"dev-node"' \
      | sed -n 's/^"id":"\([^"]*\)".*/\1/p' | head -1)"
    [ -n "$id" ] && break
    [ "$attempt" -lt 10 ] && sleep 2
  done
  if [ -z "$id" ]; then
    rm -f "$jar"
    warn "dev-node has not joined yet, so it is unlabelled and takes no build work."
    echo "  Re-run ./run.sh once it appears, set the label on the Nodes page, or:" >&2
    echo "    jar=\$(mktemp)" >&2
    echo "    curl -sS -c \"\$jar\" -X POST $CONTROL_URL/api/v1/auth/dev-login \\" >&2
    echo "      -H 'Content-Type: application/json' -d '{\"email\":\"$DEV_EMAIL\"}'" >&2
    echo "    curl -sS -b \"\$jar\" -X PUT $CONTROL_URL/api/v1/nodes/<node-id>/placement \\" >&2
    echo "      -H 'Content-Type: application/json' \\" >&2
    echo "      -d '{\"labels\":{\"role\":\"build\"},\"taints\":[]}'" >&2
    return 0
  fi
  if curl -fsS -b "$jar" -X PUT "$CONTROL_URL/api/v1/nodes/$id/placement" \
      -H 'Content-Type: application/json' \
      -d '{"labels":{"role":"build"},"taints":[]}' >/dev/null 2>&1; then
    say "dev-node labelled role=build — it takes the loop's build work."
  else
    warn "Could not label dev-node role=build — build jobs will stay queued."
  fi
  rm -f "$jar"
}

# shellcheck source=scripts/compose-project.sh
. ./scripts/compose-project.sh

say "Checking prerequisites..."
command -v docker >/dev/null || { echo "docker is required"; exit 1; }
docker compose version >/dev/null || { echo "docker compose v2 is required"; exit 1; }

# One implementation of "what a checkout is missing" — .env and the dev agent
# certificate — shared with scripts/dev-up.sh so a fresh worktree and a reset
# primary checkout cannot drift (MAIN-425).
./scripts/dev-bootstrap.sh
echo "  Edit .env to point OIDC_* at your IdP, or leave AUTH_DEV_MODE=true for dev-login."

# `-v` now takes only the DATA volumes — pgdata, minio, the seeded workspaces
# and node config. The build caches (cargo registry, both cargo targets, the
# web node_modules) are bind mounts under ./.cache, so a reset destroys the
# database without throwing away the compile cache. That is the difference
# between a reset costing seconds and costing a full cold rebuild.
say "Destroying previous environment (data volumes; build caches kept)..."
docker compose down -v --remove-orphans

# `down -v` keeps the bind-mounted caches, but the checkout can still be COLD —
# a fresh worktree, or a hand-cleaned `.cache/`. Starting the three cargo-watch
# services against a cold registry is the MAIN-425 race (the loser exits 101
# and never retries), which presents as a control plane that is "Up" and never
# serves. dev-up.sh has carried this guard since MAIN-425; run.sh went without.
say "Checking the cargo registry..."
./scripts/dev-prewarm.sh

# No type-gen here, deliberately: booting must not write tracked files. The
# committed schema.d.ts is the contract — whoever changes API types runs
# ./scripts/gen-types.sh, and CI fails on drift (it regenerates and diffs).
# Regenerating on every boot hid that responsibility, cost a host cargo build
# each run, and dragged the host toolchain (corepack, nvm) into what should
# be a docker-only path.

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

# A `node` image from before the `dev-node` stage has no `gh` and no `claude`,
# and compose will not rebuild it: it builds what is missing, not what changed.
# Probing the image itself — rather than trusting a version marker — is what
# makes this correct for a checkout that has never had the stage and for one
# whose image predates a later addition to it.
rebuild_node_image_if_toolchainless() {
  local img="${COMPOSE_PROJECT_NAME}-node"
  docker image inspect "$img" >/dev/null 2>&1 || return 0   # absent: `up` builds it
  if docker run --rm --entrypoint sh "$img" -lc \
      'command -v gh >/dev/null && command -v claude >/dev/null' >/dev/null 2>&1; then
    return 0
  fi
  say "The dev node image predates its agent toolchain — rebuilding it..."
  docker compose build node || warn "Could not rebuild the node image; build passes will fail on \`gh: not found\`."
}

if [ -n "$BUILD" ]; then
  say "Rebuilding images and starting the stack..."
else
  rebuild_node_image_if_toolchainless
  say "Starting the stack (reusing existing images — ./run.sh --build to rebuild)..."
  # Only worth saying when there IS an operator-node image that could be stale;
  # on a first run compose builds it here anyway and the warning would be a lie.
  if docker image inspect "$(docker compose config --images 2>/dev/null | grep -i operator | head -1)" \
      >/dev/null 2>&1; then
    warn "operator-node runs a BAKED binary — rebuild it after changing crates/"
  fi
fi
# shellcheck disable=SC2086  # $BUILD is a single flag or empty, by construction.
docker compose up $BUILD -d

# Compile-aware: a cold cache means cargo-watch builds for many minutes before
# the first /healthz, and the flat 120s timer here called that a failed boot.
say "Waiting for control plane (a cold cache compiles first — this can take a while)..."
./scripts/dev-wait-healthy.sh "$CONTROL_URL" || { echo "control plane failed to become healthy"; exit 1; }

# The agent the loop runs on — offered here, after the stack is healthy, so the
# operator-node is actually up to run the flow in.
claude_login_gate

# Everything below is scaffolding, not credentials: a real repo to clone and a
# CLI token minted through the dev-login hatch.
provision_dogfood_repo
refresh_dev_cli_context
label_dev_build_node

say "NookOS is up."
echo
echo "  Web UI:        $WEB_URL"
echo "  API:           $CONTROL_URL  (docs at /docs)"
echo "  MCP:           $CONTROL_URL/mcp"
echo
echo "  Fleet Claude login (specs/loops):"
echo "    ./run.sh --claude-login          # log in, or switch accounts"
echo "    docker compose exec operator-node claude auth status"
echo
echo "  Test the loop end to end (MAIN-341):"
echo "    1. ./run.sh --claude-login       # once — the only manual step"
echo "    2. open the board, pick \"Add a greeting command to the dogfood repo\""
echo "    3. its /loop page → Draft a spec"
echo
echo "  Add this machine as a node:"
echo "    cargo run -p nook-node -- join --server $CONTROL_URL --token <token from UI>"
