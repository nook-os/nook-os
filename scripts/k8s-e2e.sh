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
#   scripts/k8s-e2e.sh --pull       published control-plane + web built here — CI default
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
# How the app images get into kind: build (from source), pull (published control
# plane, web from source), or reuse (already loaded). Building the CONTROL-PLANE
# image is a full Rust release compile -- on a small CI runner it starves the
# kind node and the in-cluster Postgres cannot become Ready, so `pull` takes the
# published one (AC-4 explicitly allows "the images the release pipeline
# publishes").
#
# The WEB image is built even under `pull`, and that is the whole point of the
# mode (MAIN-654). It is a node build and an nginx copy -- seconds, not the
# thing that starved anything -- and pulling it made a chart change that is
# COUPLED to its image untestable: the chart under test is this branch's while
# the image is the last release's, so moving nginx's port failed here for as
# long as the chart was right and the image had not shipped yet. Pairing a new
# chart with an old image proves only that they were once in step.
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

# Everything needed to see WHY the app did not converge — pod state, scheduling
# events, and container logs (current AND previous, so a crash-loop shows its
# last words). Called from the cleanup trap on ANY failing exit, so a
# `helm --wait` / rollout timeout is diagnosable instead of a bare "context
# deadline exceeded" followed by the cluster being deleted (which is what made
# two earlier runs impossible to debug).
diag_dump() {
  echo "──────── FAILURE diagnostics: namespace $NS ────────"
  kube get nodes -o wide 2>/dev/null || true
  kube -n "$NS" get pods -o wide 2>/dev/null || true
  kube -n "$NS" get events --sort-by=.lastTimestamp 2>/dev/null | tail -40 || true
  kube -n "$NS" describe pods 2>/dev/null || true
  for sel in \
    "app.kubernetes.io/component=control" \
    "app.kubernetes.io/component=web" \
    "app=$PG"; do
    echo "── logs ($sel) ──"
    kube -n "$NS" logs -l "$sel" --tail=100 --all-containers 2>/dev/null || true
    kube -n "$NS" logs -l "$sel" --tail=100 --all-containers --previous 2>/dev/null || true
  done
  echo "────────────────────────────────────────────────────"
}

wait_rollout() { # $1 = deploy (name or name-of), $2 = timeout, $3 = label
  kube -n "$NS" rollout status "$1" --timeout="$2" || die "$3 did not become Ready within $2"
}

cleanup() {
  local code=$?
  [ -n "$PF_PID" ] && kill "$PF_PID" 2>/dev/null || true
  # Dump BEFORE teardown erases the evidence.
  if [ "$code" -ne 0 ]; then diag_dump || true; fi
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
wait_rollout "deploy/$PG" 180s "Postgres"

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
      log "pulling the published control plane ($PULL_TAG); building web from source"
      docker pull "$PULL_CONTROL:$PULL_TAG"
      docker tag "$PULL_CONTROL:$PULL_TAG" "$IMG_REPO/nook-control:e2e-1"
      docker build -t "$IMG_REPO/nook-web:e2e-1" -f deploy/docker/web-prod.Dockerfile .
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
# SECRETS_KEY (64 hex) too, so the crypto/vault path uses an explicit key.
kube -n "$NS" create secret generic "$SECRET" \
  --from-literal=DATABASE_URL="postgres://nook:nook@$PG:5432/nook" \
  --from-literal=SESSION_SECRET="$(openssl rand -hex 32)" \
  --from-literal=SECRETS_KEY="$(openssl rand -hex 32)"

# ── Helpers ──────────────────────────────────────────────────────────────────
install_at() { # $1 = image tag, $2... = extra --set flags
  local tag="$1"; shift
  # No `helm --wait`: its failure is an opaque "context deadline exceeded" with
  # no pod state. Apply, then wait_rollout ourselves so a stuck deployment is
  # attributed and the trap dumps its pods/events/logs.
  helm upgrade --install "$RELEASE" "$CHART" \
    --kube-context "$CTX" -n "$NS" \
    -f "$CHART/ci/e2e-values.yaml" \
    --set existingSecret="$SECRET" \
    --set controlPlane.image.tag="$tag" \
    --set web.image.tag="$tag" "$@"
}

assert_healthy() { # $1 = label for the log line
  local web ok=""
  wait_rollout \
    "$(kube -n "$NS" get deploy -l app.kubernetes.io/component=control -o name)" \
    300s "$1: control-plane"
  wait_rollout \
    "$(kube -n "$NS" get deploy -l app.kubernetes.io/component=web -o name)" \
    300s "$1: web"

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

# ── Upgrading a release that mounts the upload PVC (MAIN-653) ────────────────
# Everything above ran on the emptyDir case, which ci/e2e-values.yaml selects
# deliberately. But a real install has the PVC, and that is the configuration
# where every upgrade used to stop and wait for a human: the replacement pod
# cannot attach a ReadWriteOnce volume the outgoing pod still holds, and
# RollingUpdate does not remove the outgoing pod until the replacement is
# Ready. So turn persistence ON and upgrade twice more on top of it.
#
# TWICE, not once, because the first upgrade after a strategy change is the
# easy one — it is the SECOND that proves the release settled into a shape it
# can keep leaving, rather than one that merely survived being entered.
persist=(--set userContent.persistence.enabled=true
         --set userContent.persistence.size=1Gi)

log "helm upgrade (userContent.persistence -> enabled)"
install_at e2e-2 "${persist[@]}"
assert_healthy "persistence on"

kube -n "$NS" get deploy -l app.kubernetes.io/component=control \
  -o jsonpath='{.items[0].spec.strategy.type}' | grep -qx Recreate \
  || die "the control-plane Deployment mounts a PVC but is not set to Recreate"

# A changed config value per upgrade, so each one is a real rollout: the pod
# template carries a checksum of the ConfigMap, so an identical upgrade would
# replace nothing and prove nothing.
for lvl in debug info; do
  log "helm upgrade with the PVC mounted (logLevel=$lvl)"
  install_at e2e-2 "${persist[@]}" --set config.logLevel="$lvl"
  assert_healthy "pvc upgrade ($lvl)"
done

# Nothing above deletes a pod, so reaching here IS "no hand-deleting". This
# catches the failure by name in case a future strategy change reintroduces the
# overlap and the rollout happens to win the race anyway.
if kube -n "$NS" get events 2>/dev/null | grep -q 'Multi-Attach'; then
  die "a Multi-Attach error appeared — the rollout overlapped on the upload volume"
fi

log "chart end-to-end PASSED — install + upgrade converged and served"
