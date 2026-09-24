# syntax=docker/dockerfile:1
# Test image: keel from the working tree, debug profile. Release builds need
# more memory than a small Docker VM has, and the tests do not need the speed.
# The cargo registry and target directory are cache mounts, so a rebuild after
# a change compiles only what changed.

FROM rust:1-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends libssl-dev pkg-config cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src
COPY tests/e2e ./tests/e2e

ARG CARGO_BUILD_JOBS=3
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} cargo build --bin keel \
    && cp target/debug/keel /keel

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/log/keel /var/run/keel /etc/keel

COPY --from=builder /keel /usr/local/bin/keel

ENTRYPOINT ["/usr/local/bin/keel"]
