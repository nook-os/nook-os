#!/usr/bin/env bash
# Build the image every loop-job agent runs inside (MAIN-611) — the DEVELOPMENT
# path, not the install path.
#
# INSTALLING A NODE NEEDS NOTHING FROM HERE. Since MAIN-643 the release workflow
# publishes `ghcr.io/nook-os/nook-job-sandbox` at every version, an agent's
# default image is the tag matching its OWN version, and a node with no such
# image pulls it. Run this when you are CHANGING the sandbox and want to try the
# change before it is released — an image built here is yours, so point the node
# at it explicitly with `NOOK_SANDBOX_IMAGE`, which is never auto-pulled or
# overridden.
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
echo "This is NOT the node's default — that is the published image at the agent's own"
echo "version, which a node pulls for itself. To run jobs in this one instead:"
echo "  NOOK_SANDBOX_IMAGE=$TAG"
echo "and check it took with: nook get nodes"
