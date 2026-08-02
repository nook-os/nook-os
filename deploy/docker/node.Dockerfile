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
# `openssh-client` is not optional: git speaks ssh by forking the `ssh` binary,
# so an `git@host:org/repo` clone dies with "cannot run ssh: No such file or
# directory" without it — which is every private repo (MAIN-366).
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git tmux bash procps openssh-client \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/nook /usr/local/bin/nook
COPY deploy/docker/node-prod-entrypoint.sh /usr/local/bin/node-entrypoint.sh
RUN chmod +x /usr/local/bin/node-entrypoint.sh
ENTRYPOINT ["node-entrypoint.sh"]
