FROM rust:1-slim-bookworm AS build
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Shared cargo caches. The copy-out has to live in this RUN: target/ is a mount
# rather than image content, so it is gone by the next instruction.
RUN --mount=type=cache,id=nook-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=nook-cargo-target,target=/src/target,sharing=locked \
    cargo build --release -p nook-chat \
    && mkdir -p /out && cp target/release/nook-chat /out/

FROM debian:bookworm-slim
# curl is kept so the compose/k8s healthcheck can hit /healthz.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/nook-chat /usr/local/bin/nook-chat
RUN groupadd --system --gid 10001 nook \
    && useradd --system --uid 10001 --gid 10001 --home-dir /home/nook --create-home nook
USER 10001:10001
EXPOSE 8082
ENTRYPOINT ["nook-chat"]
