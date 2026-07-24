FROM rust:1-slim-bookworm AS build
WORKDIR /src
# curl: utoipa-swagger-ui's build script downloads the UI bundle at compile time.
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev curl ca-certificates && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The installer and the agent skill are embedded with include_str!, so they
# are build inputs, not runtime files — the build fails without them.
COPY install ./install
COPY skills ./skills
# The node agent ships *with* the control plane, not beside it. Serving the
# binary it was built alongside is what keeps a self-hosted fleet on one
# version: /install.sh can only ever hand out this build.
RUN cargo build --release -p nook-control -p nook-node
RUN mkdir -p /dist && cp target/release/nook "/dist/nook-linux-$(uname -m)"

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl openssh-client && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/nook-control /usr/local/bin/nook-control
# Anything matching nook-<os>-<arch> here is offered by /api/v1/node/releases
# and served at /dist/<name> — drop cross-built macOS binaries in to add them.
COPY --from=build /dist/ /usr/local/share/nook/dist/
# Run as a non-root, numeric UID so Kubernetes `runAsNonRoot` is satisfiable
# (it checks the USER is not 0, and a name it cannot resolve fails that check —
# hence the explicit 10001, not just a name). The binary and its read-only dist
# assets are owned by root but world-readable, so the process needs no write
# access to the image filesystem to run. Anything it does write (git checkouts,
# artifacts) already goes to mounted volumes, never the image.
RUN groupadd --system --gid 10001 nook \
    && useradd --system --uid 10001 --gid 10001 --home-dir /home/nook --create-home nook
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["nook-control"]
