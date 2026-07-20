# Dev image for Rust services: source is bind-mounted, cargo-watch rebuilds
# on change INSIDE the container. Production images live alongside
# (control.Dockerfile / node.Dockerfile) and build release binaries.
FROM rust:1-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev curl git tmux bash procps openssh-client \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-watch --locked
WORKDIR /app
