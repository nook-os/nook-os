#!/usr/bin/env bash
# Create the gitignored files a checkout needs before the stack will boot
# (MAIN-425). Idempotent: every step is skipped when its output already exists,
# so running it against the primary checkout changes nothing.
#
# This exists because a fresh worktree — which is what a nook session gets — has
# the repo's TRACKED files and nothing else, and the stack needs two things that
# are deliberately untracked:
#
#   .env                        `docker compose` refuses to start without it
#   deploy/dev-certs/agent.*    the control plane boots, then dies reading the key
#
# Both were previously obtained by hand-copying from the primary checkout, which
# is precisely what a detached loop job cannot do — so every port-lease
# acceptance test had to be argued rather than run.
#
# Preparation only: it starts nothing and never touches a database. `dev-up.sh`
# is the one command that boots a stack; `run.sh` calls this too, so there is one
# implementation of "what a checkout is missing".
set -euo pipefail

cd "$(dirname "$0")/.."

say() { printf '\033[36m▸ %s\033[0m\n' "$1"; }

# ── .env ─────────────────────────────────────────────────────────────────────
if [ -f .env ]; then
  say ".env present"
else
  say "Creating .env from .env.example"
  cp .env.example .env
fi

# Who the stack's containers run as (MAIN-537 AC-1). Every service that
# bind-mounts this checkout runs as the HOST user, so that nothing a build writes
# into its worktree is root-owned and an ordinary prune can delete it. Written
# here, once, because compose interpolates `.env` and cannot call `id`.
#
# Recorded rather than defaulted: `${NOOK_UID:-1000}` in the compose file is
# right for the ordinary first user on a Linux host and wrong for everyone else,
# and "wrong" here means a container that cannot write its own target directory.
for var in "NOOK_UID=$(id -u)" "NOOK_GID=$(id -g)"; do
  name=${var%%=*}
  if grep -qE "^${name}=" .env; then
    say "$name already set in .env"
  else
    say "Recording $var in .env"
    # A hand-edited .env may not end in a newline, and appending to its last
    # line would produce a malformed one — at which point dotenvy stops parsing
    # and everything BELOW the damage silently disappears.
    [ -s .env ] && [ -n "$(tail -c 1 .env)" ] && printf '\n' >> .env
    printf '%s\n' "$var" >> .env
  fi
done

# The operator node joins with this token; without it the node starts and can
# never join, which looks like a broken stack rather than a missing line. The
# check is here rather than only in run.sh so `dev-up.sh` inherits it.
if ! grep -qE '^NOOK_DEV_JOIN_TOKEN=.+' .env; then
  printf '\033[31m▲ NOOK_DEV_JOIN_TOKEN is unset or empty in .env\033[0m\n' >&2
  echo "  The operator node will start but cannot join the control plane." >&2
  echo "  Copy the line from .env.example, or start without it:" >&2
  echo "    docker compose up -d --scale operator-node=0" >&2
fi

# ── dev agent TLS ────────────────────────────────────────────────────────────
# Self-signed on purpose: mTLS terminates at the control plane and nodes pin
# this certificate by FINGERPRINT, computed at runtime from whatever file is
# here (deploy/docker/node-dev-entrypoint.sh). Nothing hardcodes it, so a
# regenerated pair is as good as the previous one — which is what makes
# generating it per checkout safe rather than a version-skew hazard.
#
# Both halves are generated together and both are gitignored. Committing the
# certificate while ignoring its key — which is how this used to be — hands a
# fresh worktree a certificate it has no key for, and that is the failure this
# card is about.
certs=deploy/dev-certs
if [ -f "$certs/agent.crt" ] && [ -f "$certs/agent.key" ]; then
  say "dev agent certificate present"
else
  command -v openssl >/dev/null || {
    echo "✗ openssl is required to generate the dev agent certificate" >&2
    exit 1
  }
  say "Generating a self-signed dev agent certificate"
  mkdir -p "$certs"
  # SANs match how the cert is reached: `control-plane` from inside the compose
  # network, `localhost` from the host.
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "$certs/agent.key" -out "$certs/agent.crt" \
    -days 365 -subj '/CN=control-plane' \
    -addext 'subjectAltName=DNS:control-plane,DNS:localhost' \
    2>/dev/null
  chmod 600 "$certs/agent.key"
  chmod 644 "$certs/agent.crt"
fi

# The container build caches (MAIN-425's third gitignored thing). Bind mounts,
# not named volumes, for two reasons: `docker compose down -v` cannot take them
# with it, and they no longer depend on the compose project name — one checkout
# used to warm three separate caches because run.sh and dev-up.sh derived
# different project names. Created here so Docker does not invent them.
for d in cargo-registry cargo-target cargo-target-node web-node-modules web-home; do
  mkdir -p ".cache/$d"
done

# Same reasoning for the fleet's Claude session directory: Docker creates a
# missing bind source as ROOT, and this one is inside the checkout — 21
# root-owned files in a build worktree came from exactly that (MAIN-537).
mkdir -p .nook-secrets/claude

# A checkout that ran the stack BEFORE MAIN-537 has root-owned caches, written
# by containers that were root. The services now run as you, so cargo cannot
# take its package-cache lock and the boot dies in a way that names neither this
# change nor a fix. Detected rather than repaired: the repair needs privileges
# this script must never take by itself.
if [ -n "$(find .cache .nook-secrets -mindepth 1 ! -user "$(id -u)" -print -quit 2>/dev/null)" ]; then
  printf '\033[31m▲ .cache/.nook-secrets hold files owned by another user (root, from a pre-MAIN-537 stack)\033[0m\n' >&2
  echo "  The containers now run as $(id -u):$(id -g) and cannot write them. Fix with:" >&2
  echo "    sudo chown -R $(id -u):$(id -g) .cache .nook-secrets" >&2
  echo "  (or delete .cache to rebuild the caches from scratch)" >&2
fi

say "Checkout ready — 'docker compose up -d' will start, or use ./scripts/dev-up.sh"
