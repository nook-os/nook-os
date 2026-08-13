# The shared operator node (MAIN-125): a full NookOS node image with the loop
# toolchain preinstalled, so a deployment can ship a machine that executes loop
# work without every team member running their own. Extends the plain node
# image (deploy/docker/node.Dockerfile) with git/tmux (already there) plus the
# agent runtimes: claude, hermes, codex, copilot.
#
# Versions are PINNED via build args so an image is reproducible — never install
# unbounded `latest`. Override at build time (`--build-arg CLAUDE_VERSION=…`) to
# bump. After install the build FAILS unless every expected binary resolves on
# the runtime user's PATH, so a broken/renamed installer can't ship a half-empty
# image.

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The agent skill is embedded with include_str!: a build input, not a
# runtime file.
COPY skills ./skills
# Shared cargo caches. The copy-out has to live in this RUN: target/ is a mount
# rather than image content, so it is gone by the next instruction.
RUN --mount=type=cache,id=nook-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=nook-cargo-target,target=/src/target,sharing=locked \
    cargo build --release -p nook-node \
    && mkdir -p /out && cp target/release/nook /out/

FROM debian:bookworm-slim

# Pinned toolchain versions — exact, never unbounded `latest`, so an image is
# reproducible. Override at build time (`--build-arg CLAUDE_VERSION=…`) to bump.
ARG NODE_MAJOR=22
ARG CLAUDE_VERSION=2.1.220
ARG CODEX_VERSION=0.145.0
ARG COPILOT_VERSION=1.0.75
# Hermes ships its own install script (no npm/version manifest to pin against);
# HERMES_REF is the single pin lever. Pinned to an immutable release TAG — never
# a movable ref like `stable`/`main` — so the shipped build is reproducible
# (NousResearch/hermes-agent releases). Override to bump.
ARG HERMES_REF=v2026.7.20

# Base tools: the node image's set (ca-certificates, curl, git, tmux, bash,
# procps) plus what the runtime installers need (a C toolchain some npm deps
# build against, and Node.js, added below).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git tmux bash procps gnupg xz-utils openssh-client \
    && rm -rf /var/lib/apt/lists/*

# Node.js (for the npm-published CLIs). NodeSource, pinned to a major.
RUN curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# The agent runtimes. claude/codex/copilot are npm packages; hermes ships an
# install script (the open-source Nous Research Hermes Agent).
RUN npm install -g \
      "@anthropic-ai/claude-code@${CLAUDE_VERSION}" \
      "@openai/codex@${CODEX_VERSION}" \
      "@github/copilot@${COPILOT_VERSION}" \
    && npm cache clean --force
# Hermes is installed BEST-EFFORT, and that is deliberate. It is a third-party
# install script that runs its own `npm install`, and when that timed out on
# 2026-08-13 it failed the operator-node image and with it the whole v0.6.9
# RELEASE — an optional agent runtime taking the release down with it.
#
# Absence is already a state this system models correctly:
# `capabilities::detect_runtimes` (crates/nook-node/src/capabilities.rs:59)
# probes the PATH and advertises only the runtimes actually present, so a node
# without hermes simply never offers it and the picker never shows it. Given
# that, failing the build is the wrong response to somebody else's bad minute.
#
# Three attempts, because the observed failure was a timeout rather than a
# refusal. If it still does not land the build continues and the image ships
# without hermes — visible in `nook get nodes` under capabilities.runtimes,
# which is where you would look anyway.
RUN for attempt in 1 2 3; do \
      echo "hermes install: attempt $attempt/3"; \
      if curl -fsSL --max-time 300 https://hermes-agent.nousresearch.com/install.sh \
           | HERMES_REF="${HERMES_REF}" bash; then \
        break; \
      fi; \
      echo "hermes install: attempt $attempt failed"; \
      sleep 15; \
    done; \
    if command -v hermes >/dev/null 2>&1; then \
      echo "hermes: installed"; \
    else \
      echo "WARNING: hermes did not install; this image ships without that runtime."; \
      echo "         The node will not advertise it, which is a supported state."; \
    fi

# `gh`. The review loop IS a `gh` client: `nook-review` opens by requiring
# `gh auth status` to pass and then drives `gh pr list/view/checks`. Without the
# binary a review run cannot read a single PR — and MAIN-407 provisioned the
# TOKEN into the image without ever putting the tool that reads it here.
RUN mkdir -p -m 755 /etc/apt/keyrings \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
         -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
         > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# The dev stack runs this image as the HOST's uid/gid, because it bind-mounts
# `.nook-secrets/claude` out of the checkout and a root-written session file
# there is one more thing an ordinary user's prune cannot delete (MAIN-537
# AC-1). Nothing here needs root; what it needs is for its HOME, its workspace
# root and its tmux dir to exist with permissive modes, so that the named
# volumes Docker initializes from them are writable by that uid.
#
# **Behind a build arg, and off by default.** This image is also the RELEASED
# operator node, where it runs as root and `/root` holds the node's identity,
# its `contexts.toml` and its generated SSH key — a world-writable HOME is not
# something to ship for a convenience only a local stack needs. The directories
# are created either way; only the mode is dev's.
ARG NOOK_DEV_PERMISSIVE_DIRS=0
RUN mkdir -p /root/.config/nook /workspace /var/lib/nook/tmux \
    && if [ "$NOOK_DEV_PERMISSIVE_DIRS" = "1" ]; then \
         chmod -R a+rwX /root /workspace /var/lib/nook; \
       fi

COPY --from=build /out/nook /usr/local/bin/nook
COPY deploy/docker/operator-node-entrypoint.sh /usr/local/bin/node-entrypoint.sh
RUN chmod +x /usr/local/bin/node-entrypoint.sh

# The whole point of this image is that the toolchain is present. Fail the build
# — loudly, at build time — if any expected binary is missing from PATH, so a
# renamed package or a failed installer never ships as a silently-degraded node.
RUN set -eux; for bin in git tmux ssh gh claude hermes copilot codex nook; do \
      command -v "$bin" >/dev/null || { echo "FATAL: '$bin' not on PATH"; exit 1; }; \
    done

# Record the installed toolchain versions on the image, so "what did this build
# ship?" is answerable from the image itself (docker inspect / registry).
RUN { \
      echo "nook=$(nook --version 2>/dev/null | head -1)"; \
      echo "git=$(git --version)"; \
      echo "tmux=$(tmux -V)"; \
      echo "gh=$(gh --version 2>/dev/null | head -1)"; \
      echo "node=$(node --version)"; \
      echo "claude=$(claude --version 2>/dev/null | head -1)"; \
      echo "codex=$(codex --version 2>/dev/null | head -1)"; \
      echo "copilot=$(copilot --version 2>/dev/null | head -1)"; \
      echo "hermes=$(hermes --version 2>/dev/null | head -1)"; \
    } > /etc/nook-toolchain-versions
LABEL org.opencontainers.image.title="nook-operator-node" \
      org.opencontainers.image.description="NookOS shared operator node with the loop toolchain (claude, hermes, codex, copilot) preinstalled" \
      org.opencontainers.image.source="https://github.com/nook-os/nook-os"

# This image IS the shared operator node — the entrypoint reads this to report
# the designation (capabilities.shared_operator, MAIN-125 AC-4).
ENV NOOK_SHARED_OPERATOR=1

# Which loop stages this node runs (MAIN-142). Deliberately NOT `build`: shared
# substrate reviews, specs, decomposes and runs epics; building is a person's
# own machine. Naming `build` here would change nothing anyway — the control
# plane refuses build work on any shared operator whatever it declares.
ENV NOOK_LOOP_KINDS=spec,decompose,review,epic-run
# How many loop jobs it holds at once. `0` would stop it claiming entirely.
# The fallback since MAIN-508: `nook set capacity` overrides it centrally, so
# retuning this machine no longer needs a rebuild or a restart.
ENV NOOK_MAX_LOOP_JOBS=2
ENTRYPOINT ["node-entrypoint.sh"]
