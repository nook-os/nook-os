#!/usr/bin/env bash
# Build the image every loop-job agent runs inside (MAIN-611).
#
# A host node with no such image reports `sandbox: unavailable` and CLAIMS NO
# LOOP WORK — deliberately, because the alternative is an agent driven by
# untrusted input running as your OS user. So this is the one install step a
# machine that runs builds has to do.
#
#   ./scripts/build-job-sandbox.sh                 # extends nook-operator-node:latest
#   BASE_IMAGE=ghcr.io/.../operator-node:v1 ./scripts/build-job-sandbox.sh
#   TAG=nook-job-sandbox:v2 ./scripts/build-job-sandbox.sh
#
# The base carries the agent TOOLCHAIN (claude, gh, git, node, playwright); this
# image adds the box around it. Build the base first if you do not have one:
#
#   docker build -f deploy/docker/operator-node.Dockerfile -t nook-operator-node:latest .
set -euo pipefail
cd "$(dirname "$0")/.."

BASE_IMAGE="${BASE_IMAGE:-nook-operator-node:latest}"
TAG="${TAG:-nook-job-sandbox:latest}"

if ! docker image inspect "$BASE_IMAGE" >/dev/null 2>&1; then
  echo "base image $BASE_IMAGE is not present locally — pull it, or build it with:" >&2
  echo "  docker build -f deploy/docker/operator-node.Dockerfile -t $BASE_IMAGE ." >&2
  exit 1
fi

docker build \
  --build-arg "BASE_IMAGE=$BASE_IMAGE" \
  -f deploy/docker/job-sandbox.Dockerfile \
  -t "$TAG" .

echo
echo "Built $TAG."
echo "Point the node at it with NOOK_SANDBOX_IMAGE=$TAG if that is not the default,"
echo "then check it took with: nook get nodes"
