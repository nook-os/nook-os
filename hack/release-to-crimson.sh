#!/usr/bin/env bash
#
# release-to-crimson.sh — deploy a published NookOS release to the prod control
# plane on crimson, without hand-editing anything.
#
#   ./hack/release-to-crimson.sh              # deploy the newest published release
#   ./hack/release-to-crimson.sh v0.4.17      # deploy a specific tag
#   ./hack/release-to-crimson.sh v0.4.16 -y   # skip the confirmation prompt
#   ./hack/release-to-crimson.sh --rollback   # redeploy the .env backup's previous tag
#
# Run it from your workstation (it needs `ssh crimson`, `gh`, and `curl`). It
# SSHes into crimson and does the docker work there.
#
# WHY WE PIN A TAG INSTEAD OF USING :latest
#   It is tempting to point the containers at :latest and just `pull`. Don't:
#   - Rollback stays a one-line edit (or `--rollback` here) only while the
#     running version is written down; :latest erases which build is live.
#   - `docker compose ps` keeps saying the actual version with a pinned tag.
#   - Node agents self-update to the control plane's *exact* CARGO_PKG_VERSION,
#     so the running control-plane version must be a real, specific release for
#     the fleet to converge. :latest does not change that — it just hides it.
#   This script keeps the pin but removes the manual editing: it resolves the
#   tag for you (newest release by default) and writes it into .env itself.
#
# HOST NOTE
#   crimson is a LAN host (ssh alias `crimson`, 10.12.29.201). The DNS name
#   nook.hein.network resolves to a *fronting* address (10.12.29.1), NOT the
#   deploy host — so we ssh `crimson`, never the domain.
#
set -euo pipefail

# ── Config ──────────────────────────────────────────────────────────────────
HOST="${CRIMSON_HOST:-crimson}"          # ssh alias for the deploy host
DIR="${CRIMSON_DIR:-~/domains/nook.hein.network}"
COMPOSE="docker-compose.prod.yml"
REPO="nook-os/nook-os"                    # for `gh release`
HEALTH_URL="https://nook.hein.network/healthz"

# ── Args ────────────────────────────────────────────────────────────────────
TAG=""
ASSUME_YES=0
ROLLBACK=0
for arg in "$@"; do
  case "$arg" in
    -y|--yes)     ASSUME_YES=1 ;;
    --rollback)   ROLLBACK=1 ;;
    -h|--help)    grep '^#' "$0" | sed 's/^# \{0,1\}//' | sed '/^!/d'; exit 0 ;;
    v*)           TAG="$arg" ;;
    *) echo "unknown argument: $arg (try --help)" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }
die() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# ── Rollback: read the previous tag off crimson's newest .env backup ─────────
if [[ "$ROLLBACK" == 1 ]]; then
  say "Rollback: finding the previous NOOK_TAG from crimson's latest .env backup…"
  PREV="$(ssh -o BatchMode=yes "$HOST" "cd $DIR && ls -t .env.bak-pre-* 2>/dev/null | head -1 | xargs -r grep -h '^NOOK_TAG=' | cut -d= -f2")"
  [[ -n "$PREV" ]] || die "no .env backup found on crimson to roll back to."
  TAG="$PREV"
  say "Rolling back to $TAG"
fi

# ── Resolve the tag (newest published release by default) ────────────────────
if [[ -z "$TAG" ]]; then
  say "No tag given — resolving the newest published release…"
  TAG="$(gh release view --repo "$REPO" --json tagName -q .tagName 2>/dev/null)" \
    || die "could not read the latest release from GitHub (is gh authed?)."
  [[ -n "$TAG" ]] || die "no releases found for $REPO."
fi
say "Target tag: $TAG"

# ── Verify the release actually exists (images publish with it) ──────────────
# The release workflow publishes the images and the GitHub release together, so
# a missing release means the build has not finished (or failed) — deploying
# would just 'manifest not found'. Catch it here, before touching crimson.
gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1 \
  || die "release $TAG not found on GitHub — is the release build done? (gh run list --workflow release.yml)"

# ── Confirm (prod!) ──────────────────────────────────────────────────────────
if [[ "$ASSUME_YES" != 1 ]]; then
  CURRENT="$(ssh -o BatchMode=yes "$HOST" "cd $DIR && grep -h '^NOOK_TAG=' .env | cut -d= -f2" 2>/dev/null || echo '?')"
  printf '\nDeploy \033[1m%s\033[0m to \033[1m%s:%s\033[0m (currently \033[1m%s\033[0m)? [y/N] ' "$TAG" "$HOST" "$DIR" "$CURRENT"
  read -r reply
  [[ "$reply" =~ ^[Yy]$ ]] || die "aborted."
fi

# ── Deploy (remote) ──────────────────────────────────────────────────────────
# One remote script under `set -e`: back up .env, set the tag, PULL, then only
# `up -d` if the pull succeeded (pull is non-destructive — a failed pull must
# not lead to recreating containers against images that are not there).
say "Deploying on $HOST…"
# Unquoted heredoc ON PURPOSE: $DIR, $COMPOSE and $TAG are known local values we
# bake into the remote script (and $DIR's ~ must expand on crimson, which quoting
# would prevent). Everything that must run on the server is escaped with \$.
# shellcheck disable=SC2087
ssh -o BatchMode=yes "$HOST" "bash -s" <<REMOTE
set -euo pipefail
cd $DIR
# Timestamped-by-tag backup, matching the local convention.
cp -a .env ".env.bak-pre-${TAG//\//-}"
sed -i "s/^NOOK_TAG=.*/NOOK_TAG=$TAG/" .env
echo "  NOOK_TAG set to \$(grep '^NOOK_TAG=' .env | cut -d= -f2)"
echo "  Pulling images…"
docker compose -f $COMPOSE pull
echo "  Recreating containers…"
docker compose -f $COMPOSE up -d
echo "  Running images:"
docker compose -f $COMPOSE ps --format '    {{.Service}}  {{.Image}}  {{.Status}}' | grep -E 'control|web|chat|postgres'
REMOTE

# ── Verify ───────────────────────────────────────────────────────────────────
say "Verifying $HEALTH_URL…"
for i in $(seq 1 15); do
  code="$(curl -s -o /dev/null -w '%{http_code}' "$HEALTH_URL" || true)"
  [[ "$code" == "200" ]] && { printf '  healthz: \033[1;32m200 OK\033[0m\n'; break; }
  printf '  healthz: %s (retry %d/15)\n' "$code" "$i"; sleep 2
done
[[ "${code:-}" == "200" ]] || die "control plane did not come healthy — check: ssh $HOST 'cd $DIR && docker compose -f $COMPOSE logs control-plane --tail 40'"

cat <<DONE

✓ Deployed $TAG to $HOST.
  Nodes self-update from the control plane — watch them converge:
      nook get nodes        # AGENT_VERSION should roll to ${TAG#v}
  Roll back if needed:
      ./hack/release-to-crimson.sh --rollback
DONE
