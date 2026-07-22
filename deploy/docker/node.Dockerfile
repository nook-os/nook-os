FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The agent skill is embedded with include_str!: a build input, not a
# runtime file.
COPY skills ./skills
RUN cargo build --release -p nook-node

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git tmux bash procps \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/nook /usr/local/bin/nook
COPY deploy/docker/node-prod-entrypoint.sh /usr/local/bin/node-entrypoint.sh
RUN chmod +x /usr/local/bin/node-entrypoint.sh
ENTRYPOINT ["node-entrypoint.sh"]
