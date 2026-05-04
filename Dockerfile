#  Build stage
FROM rust:1-bookworm AS builder

WORKDIR /build

RUN apt-get update \
    && apt-get install -y libssl-dev pkg-config cmake \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

#  Runtime stage
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/log/keel /var/run/keel /etc/keel

COPY --from=builder /build/target/release/keel /usr/local/bin/keel

EXPOSE 80 443 9090

ENTRYPOINT ["/usr/local/bin/keel"]
CMD ["--config", "/etc/keel/keel.yaml"]
