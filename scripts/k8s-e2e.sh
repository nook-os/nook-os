#!/usr/bin/env bash
# End-to-end test of the nook-control Helm chart on a real (local) Kubernetes
# cluster — the proof that `helm template` cannot give.
#
# It stands up a `kind` cluster, deploys a throwaway Postgres inside it, creates
# the Secret the chart references, `helm install`s the LOCAL chart against that
# Postgres, waits for both Deployments to be Ready, and asserts the app answers
# — the SPA loads and `/healthz` passes (proxied web -> control -> Postgres, so
# a green run proves the whole path). Then it exercises `helm upgrade` (install
# one image tag, roll to another, re-assert) and tears the cluster down.
#
# Everything happens inside kind and over an ephemeral port-forward on 18080, so
# it never binds host 8080/8081 and cannot collide with a running dev stack.
#
# Idempotent and self-cleaning: it deletes any stale cluster of the same name
# before starting, and removes the cluster on exit unless --keep is given.
#
#   scripts/k8s-e2e.sh              build images from source, run cycle, tear down
#   scripts/k8s-e2e.sh --pull       use PUBLISHED images (no in-job compile) — CI default
#   scripts/k8s-e2e.sh --pull-tag T published tag to pull (default: latest)
#   scripts/k8s-e2e.sh --keep       leave the cluster up for debugging
#   scripts/k8s-e2e.sh --no-build   reuse images already tagged + loaded
#   scripts/k8s-e2e.sh --cluster X  use a differently named kind cluster
set -euo pipefail
cd "$(dirname "$0")/.."

CLUSTER=${CLUSTER:-nook-e2e}
NS=nook-e2e
RELEASE=nook
SECRET=nook-e2e-secrets
PG=nook-e2e-postgres
IMG_REPO=nook.local
PF_PORT=18080
CHART=charts/nook-control
KEEP=0
# How the app images get into kind: build (from source), pull (published), or
# reuse (already loaded). Building the control-plane image is a full Rust
# release compile — on a small CI runner it starves the kind node and the
# in-cluster Postgres cannot become Ready, so CI pulls published images instead
# (AC-4 explicitly allows "the images the release pipeline publishes").
MODE=build
PULL_CONTROL=${PULL_CONTROL:-ghcr.io/nook-os/nook-control}
PULL_WEB=${PULL_WEB:-ghcr.io/nook-os/nook-web}
PULL_TAG=${PULL_TAG:-latest}
PF_PID=""

usage() { sed -n '18,23p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    --build) MODE=build ;;
    --pull) MODE=pull ;;
    --pull-tag) PULL_TAG="${2:?--pull-tag needs a tag}"; shift ;;
    --no-build) MODE=reuse ;;
    --cluster) CLUSTER="${2:?--cluster needs a name}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
  esac
  shift
done

for t in kind kubectl helm docker openssl curl; do
  command -v "$t" >/dev/null 2>&1 || { echo "missing required tool: $t" >&2; exit 1; }
done

CTX="kind-$CLUSTER"
kube() { kubectl --context "$CTX" "$@"; }
log()  { printf '\n\033[38;5;214m▸\033[0m %s\n' "$*"; }
die()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# On any convergence failure, show WHY before dying — pod state, recent events,
# and logs — so a CI failure is diagnosable from the run log instead of a bare
# "context deadline exceeded".
dump_diag() { # $1 = a selector or "" for the whole namespace
  echo "── diagnostics: namespace $NS ─────────────────────────────────────────"
  kube -n "$NS" get pods -o wide || true
  kube -n "$NS" get events --sort-by=.lastTimestamp | tail -25 || true
  if [ -n "${1:-}" ]; then
    kube -n "$NS" describe pods -l "$1" || true
    kube -n "$NS" logs -l "$1" --tail=40 --all-containers 2>/dev/null || true
  fi
  echo "───────────────────────────────────────────────────────────────────────"
}

wait_rollout() { # $1 = deploy (name or name-of), $2 = timeout, $3 = selector, $4 = label
  if ! kube -n "$NS" rollout status "$1" --timeout="$2"; then
    dump_diag "$3"
    die "$4 did not become Ready within $2"
  fi
}

cleanup() {
  local code=$?
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  if [ "$KEEP" = 1 ]; then
    log "keeping cluster '$CLUSTER' (--keep)"
    echo "  inspect: kubectl --context $CTX get pods -n $NS"
    echo "  delete:  kind delete cluster --name $CLUSTER"
  else
    log "tearing down cluster '$CLUSTER'"
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  fi
  exit "$code"
}
trap cleanup EXIT INT TERM

# ── Cluster (idempotent: a prior failed run leaves nothing behind) ───────────
log "removing any stale '$CLUSTER' cluster"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
log "creating kind cluster '$CLUSTER'"
kind create cluster --name "$CLUSTER" --wait 120s

# ── Throwaway Postgres FIRST, on an unburdened node ──────────────────────────
# Before any image work: when images are built from source (the local default),
# the compile is heavy, so getting Postgres Ready first means its readiness never
# competes with the build. On a pull run it is simply fast.
log "creating namespace + throwaway Postgres"
kube create namespace "$NS"
kube -n "$NS" apply -f "$CHART/ci/postgres.yaml"
wait_rollout "deploy/$PG" 180s "app=$PG" "Postgres"

# ── Images: build or pull, tag as two versions, load into kind ───────────────
# The upgrade test (AC-3) needs two tags; the same image under two tags proves
# the rollout rolls and the app stays reachable across an upgrade without
# requiring two genuinely different builds.
prepare_source() {
  case "$MODE" in
    build)
      log "building images from source (control-plane is a full Rust compile)"
      docker build -t "$IMG_REPO/nook-control:e2e-1" -f deploy/docker/control.Dockerfile .
      docker build -t "$IMG_REPO/nook-web:e2e-1" -f deploy/docker/web-prod.Dockerfile .
      ;;
    pull)
      log "pulling published images ($PULL_TAG) — no in-job compile"
      docker pull "$PULL_CONTROL:$PULL_TAG"
      docker pull "$PULL_WEB:$PULL_TAG"
      docker tag "$PULL_CONTROL:$PULL_TAG" "$IMG_REPO/nook-control:e2e-1"
      docker tag "$PULL_WEB:$PULL_TAG" "$IMG_REPO/nook-web:e2e-1"
      ;;
    reuse)
      log "reusing already-loaded images"
      return 0
      ;;
  esac
  docker tag "$IMG_REPO/nook-control:e2e-1" "$IMG_REPO/nook-control:e2e-2"
  docker tag "$IMG_REPO/nook-web:e2e-1" "$IMG_REPO/nook-web:e2e-2"
}
prepare_source

log "loading images into kind"
kind load docker-image --name "$CLUSTER" \
  "$IMG_REPO/nook-control:e2e-1" "$IMG_REPO/nook-control:e2e-2" \
  "$IMG_REPO/nook-web:e2e-1" "$IMG_REPO/nook-web:e2e-2"

log "creating the chart's Secret (DATABASE_URL -> in-cluster Postgres)"
kube -n "$NS" create secret generic "$SECRET" \
  --from-literal=DATABASE_URL="postgres://nook:nook@$PG:5432/nook" \
  --from-literal=SESSION_SECRET="$(openssl rand -hex 32)"

# ── Helpers ──────────────────────────────────────────────────────────────────
install_at() { # $1 = image tag
  helm upgrade --install "$RELEASE" "$CHART" \
    --kube-context "$CTX" -n "$NS" \
    -f "$CHART/ci/e2e-values.yaml" \
    --set existingSecret="$SECRET" \
    --set controlPlane.image.tag="$1" \
    --set web.image.tag="$1" \
    --wait --timeout 200s
}

assert_healthy() { # $1 = label for the log line
  local web ok=""
  wait_rollout \
    "$(kube -n "$NS" get deploy -l app.kubernetes.io/component=control -o name)" \
    200s "app.kubernetes.io/component=control" "$1: control-plane"
  wait_rollout \
    "$(kube -n "$NS" get deploy -l app.kubernetes.io/component=web -o name)" \
    200s "app.kubernetes.io/component=web" "$1: web"

  web="$(kube -n "$NS" get svc -l app.kubernetes.io/component=web -o name | head -1)"
  kube -n "$NS" port-forward "$web" "$PF_PORT:80" >/dev/null 2>&1 &
  PF_PID=$!
  for _ in $(seq 1 30); do
    curl -fsS "http://127.0.0.1:$PF_PORT/healthz" >/dev/null 2>&1 && { ok=1; break; }
    sleep 1
  done
  [ -n "$ok" ] || die "$1: /healthz never came up through the web proxy"

  log "$1: asserting /healthz (web -> control -> Postgres)"
  curl -fsS "http://127.0.0.1:$PF_PORT/healthz" | grep -q '"status":"ok"' \
    || die "$1: /healthz did not report ok"
  log "$1: asserting the SPA loads"
  curl -fsS "http://127.0.0.1:$PF_PORT/" | grep -q '<div id="root">' \
    || die "$1: the SPA index did not render"

  kill "$PF_PID" 2>/dev/null || true
  PF_PID=""
}

# ── Install, assert, then exercise the upgrade path ──────────────────────────
log "helm install (image tag e2e-1)"
install_at e2e-1
assert_healthy "install"

log "helm upgrade (image tag e2e-1 -> e2e-2)"
install_at e2e-2
assert_healthy "upgrade"

log "chart end-to-end PASSED — install + upgrade converged and served"
