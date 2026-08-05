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

say "Checkout ready — 'docker compose up -d' will start, or use ./scripts/dev-up.sh"
