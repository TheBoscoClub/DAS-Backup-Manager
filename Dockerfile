# Stage 1: Build btrdasd
FROM rust:1.93-bookworm AS builder

WORKDIR /src
COPY indexer/ indexer/
COPY scripts/ scripts/
RUN cargo build --release --manifest-path indexer/Cargo.toml

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    btrfs-progs \
    smartmontools \
    bash \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/indexer/target/release/btrdasd /usr/local/bin/btrdasd

ENTRYPOINT ["btrdasd"]
