# Dev image for Rust services: source is bind-mounted, cargo-watch rebuilds
# on change INSIDE the container. Production images live alongside
# (control.Dockerfile / node.Dockerfile) and build release binaries.
FROM rust:1-slim-bookworm AS dev-rust
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev curl git tmux bash procps openssh-client \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-watch --locked
# `./test.sh rust` runs through nextest, in here, and CI runs the same suite the
# same way (MAIN-656) — so the runner has to be in the image or the default dev
# path breaks. The prebuilt binary rather than `cargo install`: it is seconds
# instead of minutes, and this is the tool that decides whether the tests can
# run at all.
RUN curl -fsSL https://get.nexte.st/latest/linux \
      | tar zxf - -C "$CARGO_HOME/bin" \
    && cargo nextest --version

# Usable by whatever UID compose runs this as (MAIN-537 AC-1). The services in
# this stack write into a bind-mounted checkout, so they run as the HOST user —
# which owns nothing in the image, and cargo's first act is to take the package
# cache lock inside CARGO_HOME. World-writable is a dev-image trade nobody makes
# in production: the alternative is baking a uid, and the host's is not knowable
# at build time.
RUN chmod -R a+rwX "$CARGO_HOME"

# The upload directory's mount point (MAIN-598), for the same reason and by the
# same mechanism: Docker seeds a named volume from the image directory it is
# mounted over, so a mount point absent from the image arrives root-owned and
# the non-root service cannot write it.
RUN mkdir -p /var/lib/nook/user-content && chmod -R a+rwX /var/lib/nook

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
ARG PLAYWRIGHT_VERSION=1.62.1

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

# Playwright and Chromium — ONLY Chromium (MAIN-595). This stage gets it and
# `node.Dockerfile` does not, because THIS is the image build runs execute on:
# the `node` service declares `NOOK_LOOP_KINDS=build`, and the control plane
# refuses build work on the shared operator outright
# (`jobs::kind_wall_refusal`), so a browser only reaches a build run here.
#
# Fixed browsers path, world-readable, for the same reason as the operator
# image: root installs them at build time and the host's uid launches them.
ENV PLAYWRIGHT_BROWSERS_PATH=/ms-playwright
ENV NOOK_PLAYWRIGHT_VERSION=${PLAYWRIGHT_VERSION}
RUN npm install -g "playwright@${PLAYWRIGHT_VERSION}" \
    && playwright install --with-deps chromium \
    && npm cache clean --force \
    && chmod -R a+rX /ms-playwright \
    && rm -rf /var/lib/apt/lists/*

# Launched during the build, so an image whose Chromium cannot start fails here
# rather than at the first build run that wanted a page. Re-runnable against a
# live container: `docker compose exec node nook-browser-check`.
COPY deploy/docker/browser-check.js /usr/local/bin/nook-browser-check
RUN chmod +x /usr/local/bin/nook-browser-check && nook-browser-check

# What the entrypoint and the node write as the host user (MAIN-537 AC-1): the
# `nook` symlink it re-makes on every start, its HOME, and the mount points of
# the two named volumes. A volume Docker initializes from a directory that is
# not there arrives root-owned and unwritable, so these exist HERE rather than
# being created by the first container to want them.
RUN mkdir -p /root/.config/nook /workspace \
    && chmod -R a+rwX /root /workspace /usr/local/bin

# Fail the BUILD if the toolchain is incomplete, as operator-node.Dockerfile
# does — a renamed package or a failed installer must never ship as a node that
# looks healthy and cannot run a pass. `nook` is deliberately absent here: it is
# built from the bind-mounted source and linked onto PATH by the entrypoint.
RUN set -eux; for bin in git tmux ssh gh claude node playwright nook-browser-check; do \
      command -v "$bin" >/dev/null || { echo "FATAL: '$bin' not on PATH"; exit 1; }; \
    done
