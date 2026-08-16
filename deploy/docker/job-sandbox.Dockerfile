# The image every loop-job agent runs inside (MAIN-611).
#
# A loop agent's instructions are UNTRUSTED INPUT — a card body, a PR comment, a
# dependency's README — and before this image the agent ran as the node's OS
# user with that user's whole world. So it runs here instead: only its checkout
# mounted, a private /tmp, an egress policy the node installs, and — for the
# kinds that declare they need it — its OWN Docker daemon.
#
# It EXTENDS the operator-node image rather than rebuilding its toolchain: that
# image is already the definition of "what a loop agent needs installed", down
# to the PATH check that fails the build when a runtime goes missing. Keeping
# one definition is what stops a job container and a shared operator drifting
# into two subtly different worlds.
ARG BASE_IMAGE=nook-operator-node:latest
FROM ${BASE_IMAGE}

USER root

# Pinned like everything else in this tree — never unbounded `latest`.
ARG DOCKER_VERSION=5:29.7.2-1~debian.12~bookworm
ARG COMPOSE_PLUGIN_VERSION=5.4.0-1~debian.12~bookworm
ARG BUILDX_PLUGIN_VERSION=0.36.1-1~debian.12~bookworm
ARG CONTAINERD_VERSION=2.3.3-1~debian.12~bookworm

# `iptables` and `iproute2` ARE the egress policy of AC-5 — the node installs
# the rules with `docker exec`, so they have to exist in here. `passwd` and
# `util-linux` are what the entrypoint uses to create the agent's uid; `procps`
# for the readiness wait; `dnsutils` so `nslookup` can answer "is DNS actually
# working in here", which is the first thing anyone asks of AC-5's policy.
RUN apt-get update && apt-get install -y --no-install-recommends \
      iptables iproute2 iputils-ping dnsutils \
      procps util-linux passwd ca-certificates curl gnupg \
    && rm -rf /var/lib/apt/lists/*

# Docker itself, from Docker's own repository — the daemon, not just the client,
# because AC-4's whole point is that the job talks to a daemon of its own.
RUN install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg \
         -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable" \
         > /etc/apt/sources.list.d/docker.list \
    && apt-get update && apt-get install -y --no-install-recommends \
         "docker-ce=${DOCKER_VERSION}" \
         "docker-ce-cli=${DOCKER_VERSION}" \
         "docker-compose-plugin=${COMPOSE_PLUGIN_VERSION}" \
         "docker-buildx-plugin=${BUILDX_PLUGIN_VERSION}" \
         "containerd.io=${CONTAINERD_VERSION}" \
    && rm -rf /var/lib/apt/lists/*

# The daemon is started by the entrypoint, per job, and only for a kind whose
# profile asks for one (AC-12). Nothing here is a service.
RUN systemctl disable docker.service docker.socket containerd.service 2>/dev/null || true

# The agent's HOME. Not the host's — the host's home directory is absent from
# this container by design, and pointing HOME at a path Docker scaffolded for a
# bind mount would put dotfiles inside the bind.
RUN mkdir -p /home/agent /nook-claude && chmod 0777 /home/agent

COPY deploy/docker/job-sandbox-entrypoint.sh /usr/local/bin/job-sandbox-entrypoint.sh
RUN chmod +x /usr/local/bin/job-sandbox-entrypoint.sh

# Fail the BUILD, not the first job, if a piece of the sandbox is missing.
#
# What THIS layer adds, and only that: the agent toolchain is the base image's
# contract, and operator-node.Dockerfile already fails its own build when
# claude/gh/git go missing. Re-asserting it here would duplicate that check and,
# worse, make this file unbuildable on any other base — which is exactly how the
# escape suite of AC-11 runs it, against a bare Debian, to test the box rather
# than the toolchain inside it.
RUN set -eux; for bin in dockerd docker iptables ip nslookup; do \
      command -v "$bin" >/dev/null || { echo "FATAL: '$bin' not on PATH"; exit 1; }; \
    done

LABEL org.opencontainers.image.title="nook-job-sandbox" \
      org.opencontainers.image.description="The per-job container a NookOS loop agent runs inside (MAIN-611)" \
      org.opencontainers.image.source="https://github.com/nook-os/nook-os"

# Deliberately NOT the operator node's designation: this image executes no loop
# work of its own, it is the box one runs in.
ENV NOOK_SHARED_OPERATOR=""
ENV NOOK_LOOP_KINDS=""

ENTRYPOINT ["/usr/local/bin/job-sandbox-entrypoint.sh"]
