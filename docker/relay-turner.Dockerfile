# syntax=docker/dockerfile:1
# relay-crank-turner from the public relay repository at RELAY_REV, the revision
# programs/velocity/Cargo.toml pins for relay-spec.
FROM rust:1.95.0 AS builder
ARG RELAY_REV
RUN git clone https://github.com/velocity-exchange/relay /relay \
 && git -C /relay checkout --detach "${RELAY_REV:?RELAY_REV is required}"
WORKDIR /relay
RUN cargo build --release --locked -p relay-crank-turner \
 && cp target/release/relay-crank-turner /usr/local/bin/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/relay-crank-turner /usr/local/bin/
RUN useradd -r -u 1001 -g 0 -m -d /home/nonroot nonroot
USER 1001
ENTRYPOINT ["relay-crank-turner"]
