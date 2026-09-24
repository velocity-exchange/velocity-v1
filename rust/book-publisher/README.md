# book-publisher

The order-book publisher, the Rust half of the dlob-server split. It writes L2 book data and per-user
CLOB order state to Redis; the TypeScript `apps/dlob-server` reads the same keys and serves them
over HTTP and websocket.

## Dependents

None inside this repo depend on it as a library. `apps/dlob-server` depends on it operationally: it
reads the Redis keys this binary writes and serves no book state of its own. `test-scripts/run-e2e-localnet.sh`
builds and runs it as part of the local e2e stack, and `docker-info.json` ships it as the
`book-publisher` image.

## Build and run

```
cargo build --manifest-path rust/Cargo.toml -p book-publisher --release
./rust/target/release/book-publisher --help
```

Required flags (or the matching env var): `--rpc-url` (`RPC_URL`), `--velocity-program`
(`VELOCITY_PROGRAM_ID`), `--keypair-path` (`KEYPAIR_PATH`, the payer and quote-buffer authority).

Notable optional flags: `--transport` (`TRANSPORT`, `rpc`/`ws`/`grpc`, default `rpc`), `--markets`
(`MARKETS`, comma-separated perp market indexes, default `0`), `--redis-url` (`REDIS_URL`,
default `redis://127.0.0.1:6379`), `--tick-ms` (`TICK_MS`, default `200`), `--quote-size`
(`QUOTE_SIZE`, base precision, default `1000000000000`), `--local-sim-pool` (`LOCAL_SIM_POOL`,
pooled in-process SVM instances, `0` simulates over RPC instead, default `4`), `--cross-match`
(`CROSS_MATCH`, submit `crank_cross_match` when a tick's books cross net of fees, default `true`),
`--metrics-addr` (`METRICS_ADDR`, default `0.0.0.0:9464`).

## Design

Book production lives here; book serving stays in the TypeScript dlob-server, which holds the HTTP
endpoints, the websocket fan-out and the auth. The payload is the existing L2 wire shape, with
`clob` and `propamm` joining `vamm` in the per-level `sources` breakdown, so consumers do not move.

Simulation is the design rather than an optimization. A Custom quoter is an arbitrary program with
no off-chain decoder, so running `quote_v0` through `velocity-router-sim` is the only way to price
it. The view also runs sources in fill order, so a published book equals the fill-time book by
construction, margin-clamped PropAMMs and vAMM last-look shading included.

`src/user_orders.rs` decodes each CLOB market account with `crates/clob-state` to answer which
orders a user holds on the book, the one question no CLOB instruction answers at the tick rate a
live feed needs. `src/cross.rs` drives the `crank_cross_match` fast path when `--cross-match` finds
a tick's books crossed.
