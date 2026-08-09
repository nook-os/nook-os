# Dev image for Rust services: source is bind-mounted, cargo-watch rebuilds
# on change INSIDE the container. Production images live alongside
# (control.Dockerfile / node.Dockerfile) and build release binaries.
FROM rust:1-slim-bookworm AS dev-rust
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev curl git tmux bash procps openssh-client \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-watch --locked
WORKDIR /app

# The dev NODE, on top of the same base: the loop toolchain a build run shells
# out to (MAIN-486). Only the `node` service targets this stage, so the control
# plane, chat and worker images stay exactly as small as they were.
#
# Dev-only (NG-1). How a real deployment provisions a build environment is the
# build-execution-environments epic's problem; this exists so the dev stack can
# dogfood a build loop without anyone installing packages into a running
# container — which was done by hand three times on 2026-08-09 and lost to
# `docker compose up -d node` every time.
FROM dev-rust AS dev-node

# Pinned, never `latest`, so the image is reproducible — the same rule (and the
# same claude version) as operator-node.Dockerfile. Override to bump.
ARG NODE_MAJOR=22
ARG CLAUDE_VERSION=2.1.220

# Node.js is `claude`'s runtime, not a project toolchain: it is here because the
# npm-published CLI needs it, which is the same reason the operator node carries
# it, and NOT the start of installing language toolchains for the code under
# test (NG-2).
RUN apt-get update && apt-get install -y --no-install-recommends gnupg \
    && curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*
RUN npm install -g "@anthropic-ai/claude-code@${CLAUDE_VERSION}" \
    && npm cache clean --force

# `gh`. A build run opens a PR and reads its checks, so without the binary the
# pass dies at the point it would have shipped something.
RUN mkdir -p -m 755 /etc/apt/keyrings \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
         -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
         > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# Fail the BUILD if the toolchain is incomplete, as operator-node.Dockerfile
# does — a renamed package or a failed installer must never ship as a node that
# looks healthy and cannot run a pass. `nook` is deliberately absent here: it is
# built from the bind-mounted source and linked onto PATH by the entrypoint.
RUN set -eux; for bin in git tmux ssh gh claude node; do \
      command -v "$bin" >/dev/null || { echo "FATAL: '$bin' not on PATH"; exit 1; }; \
    done
