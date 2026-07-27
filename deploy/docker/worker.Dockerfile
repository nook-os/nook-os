FROM rust:1-slim-bookworm AS build
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p nook-worker

FROM debian:bookworm-slim
# No curl / EXPOSE: the worker has no HTTP surface — it drains the queue and
# reports liveness through its logs, not a health endpoint.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/nook-worker /usr/local/bin/nook-worker
RUN groupadd --system --gid 10001 nook \
    && useradd --system --uid 10001 --gid 10001 --home-dir /home/nook --create-home nook
USER 10001:10001
ENTRYPOINT ["nook-worker"]
