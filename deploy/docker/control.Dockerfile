FROM rust:1-slim-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p nook-control

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl openssh-client && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/nook-control /usr/local/bin/nook-control
EXPOSE 8080
ENTRYPOINT ["nook-control"]
