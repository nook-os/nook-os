FROM rust:1-slim-bookworm AS build
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p nook-chat

FROM debian:bookworm-slim
# curl is kept so the compose/k8s healthcheck can hit /healthz.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/nook-chat /usr/local/bin/nook-chat
RUN groupadd --system --gid 10001 nook \
    && useradd --system --uid 10001 --gid 10001 --home-dir /home/nook --create-home nook
USER 10001:10001
EXPOSE 8082
ENTRYPOINT ["nook-chat"]
