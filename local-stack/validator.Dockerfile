# solana-test-validator and the solana CLI, for the local stack.
#
# Anza publishes no Linux arm64 release, and the x86_64 release cannot run
# under Rosetta: agave asserts io_uring support at startup, and Rosetta does
# not implement it. An arm64 host therefore builds agave from source, once.
# An amd64 host downloads the release.
ARG AGAVE_VERSION=v4.3.0

FROM rust:1.90-bookworm AS binaries
ARG AGAVE_VERSION
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends \
      clang libclang-dev llvm-dev cmake libudev-dev libssl-dev pkg-config protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/agave-target \
    set -eu; mkdir -p /out; \
    if [ "$TARGETARCH" = amd64 ]; then \
      curl -sSfL "https://github.com/anza-xyz/agave/releases/download/${AGAVE_VERSION}/solana-release-x86_64-unknown-linux-gnu.tar.bz2" | tar -xj -C /tmp; \
      cp /tmp/solana-release/bin/solana-test-validator /tmp/solana-release/bin/solana /tmp/solana-release/bin/solana-keygen /out/; \
    else \
      git clone --depth 1 --branch "$AGAVE_VERSION" https://github.com/anza-xyz/agave /agave; \
      cd /agave && CARGO_TARGET_DIR=/agave-target cargo build --release \
        --bin solana-test-validator --bin solana --bin solana-keygen; \
      cp /agave-target/release/solana-test-validator /agave-target/release/solana /agave-target/release/solana-keygen /out/; \
    fi

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl bzip2 libssl3 libudev1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=binaries /out/ /usr/local/bin/
