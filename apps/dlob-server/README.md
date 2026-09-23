<div align="center">
  <img height="120" src="https://docs.velocity.exchange/assets/velocity.svg" />

  <h1>DLOB server</h1>

  <p>
    <a href="https://docs.velocity.exchange/developers/ecosystem-builders/orderbook-and-ws"><img alt="Docs" src="https://img.shields.io/badge/docs-developers-blueviolet" /></a>
    <a href="https://discord.com/invite/95kByNnDy5"><img alt="Discord Chat" src="https://img.shields.io/discord/849494028176588802?color=blueviolet" /></a>
    <a href="https://opensource.org/licenses/Apache-2.0"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blueviolet" /></a>
  </p>
</div>

This service serves the Velocity [order book](https://docs.velocity.exchange/protocol/how-it-works/orderbook-and-keepers)
to clients. It holds no order state of its own. The rust `book-publisher` (`rust/book-publisher`)
quotes every source through the velocity router view and writes each perp market's book to Redis.
This service reads those keys. It runs in two modes that share the same codebase.

In HTTP mode, `src/index.ts` keeps a `VelocityClient` and a slot subscriber for market metadata,
oracle data, and book freshness, and answers REST requests from the Redis keys the publisher
writes.

In websocket mode, `src/wsConnectionManager.ts` accepts client connections and relays the latest
book for the channels a client subscribed to. The publisher and the connection manager talk only
through Redis pub/sub, so you can scale them independently.

# Run the server

## Setup

Install dependencies once at the repo root, then build this app:

```bash
bun install
bunx turbo run build --filter=@velocity-exchange/dlob-server
```

Copy the example environment file and fill it in. Every command below this point runs from
`apps/dlob-server`:

```bash
cp .env.example .env
```

## Environment variables

| Variable                              | Description                                                                     | Default            |
| ------------------------------------- | ------------------------------------------------------------------------------- | ------------------ |
| `ENDPOINT`                            | Solana RPC HTTP endpoint.                                                        | none, required     |
| `WS_ENDPOINT`                         | Solana RPC websocket endpoint.                                                   | derived by web3.js |
| `ENV`                                 | Network to connect to, `devnet` or `mainnet-beta`.                               | `devnet`           |
| `PORT`                                | Port the HTTP server listens on.                                                 | `6969`             |
| `METRICS_PORT`                        | Port the Prometheus exporter listens on.                                         | `9464`             |
| `MAX_BOOK_SLOT_LAG`                   | Slots a published book may trail the chain before it counts as behind.           | `150`              |
| `BOOK_FRESHNESS_INTERVAL_MS`          | Milliseconds between samples of the published books for staleness.               | `5000`             |
| `ENABLE_FILL_QUALITY_ANALYTICS`       | Set `true` to poll Athena for taker fill quality. Needs Athena credentials.      | `false`            |
| `ELASTICACHE_HOST`                    | Redis host. In cluster mode this is the seed node and the rest are discovered.   | `localhost`        |
| `ELASTICACHE_PORT`                    | Redis port.                                                                      | `6379`             |
| `REDIS_CLIENT`                        | Comma separated key prefixes to use, from `DLOB` and `DLOB_HELIUS`.              | see below          |
| `LOCAL_CACHE`                         | Set `true` alongside `RUNNING_LOCAL` to talk to a local Redis cluster.           | unset              |
| `RUNNING_LOCAL`                       | Set `true` when Redis runs on the same machine.                                  | unset              |
| `WS_PORT`                             | Port the websocket connection manager listens on.                                | `3000`             |

When `REDIS_CLIENT` is unset the HTTP server and the connection manager open clients for both
`DLOB` and `DLOB_HELIUS`.

## HTTP mode

Start the HTTP server. It listens on `http://127.0.0.1:6969` unless you set `PORT`.

```bash
bun run dev
```

The endpoints and their response shapes are documented in the
[orderbook and websocket docs](https://docs.velocity.exchange/developers/ecosystem-builders/orderbook-and-ws).

`src/serverLite.ts` is a trimmed variant that serves only `/health`, `/startup`, `/` and `/l3`
straight out of Redis, without subscribing to markets. Run it with `bun run server-lite`.

## Websocket mode

Websocket mode needs a Redis instance. Point `ELASTICACHE_HOST` and `ELASTICACHE_PORT` at it, and
set `RUNNING_LOCAL=true` plus `LOCAL_CACHE=true` when that Redis is a local cluster.

In the first terminal, start the Redis cluster:

```bash
bash redisCluster.sh start
bash redisCluster.sh create
```

In the second terminal, run the publisher. `rust/` is its own cargo workspace, which the root one
excludes, so it is reached by manifest path from the repo root:

```bash
cargo run --manifest-path rust/Cargo.toml -p book-publisher
```

The publisher's `REDIS_KEY_PREFIX` must equal the prefix of the server's Redis client, which is
`dlob:` for `REDIS_CLIENT=DLOB`. The publisher defaults to no prefix, so every read misses until
it is set.

In a third terminal, run the connection manager:

```bash
bun run ws-manager
```

Clients then connect to `ws://127.0.0.1:3000/ws`.

When you are done, stop the cluster:

```bash
bash redisCluster.sh stop
```

# Client examples

## HTTP

Every order book route identifies a market either by `marketName`, or by `marketIndex` and
`marketType` together. `marketType` is `perp` or `spot`. Supplying neither returns 400.

Check liveness. This returns unhealthy when the slot source stops advancing, and `/startup` reports
whether the initial subscription finished:

```bash
curl 'http://127.0.0.1:6969/health'
```

Fetch an aggregated L2 book. `depth` is clamped to 100 and defaults to 100:

```bash
curl 'http://127.0.0.1:6969/l2?marketName=SOL-PERP&depth=10'
curl 'http://127.0.0.1:6969/l2?marketType=perp&marketIndex=0&depth=10'
```

Fetch several L2 books in one request. Each query parameter is a comma separated list, and all
lists must be the same length:

```bash
curl 'http://127.0.0.1:6969/batchL2?marketName=SOL-PERP,BTC-PERP&depth=5,5'
```

Fetch the order-level L3 book. This route is served only from Redis and returns 500 when no
snapshot has been published yet:

```bash
curl 'http://127.0.0.1:6969/l3?marketName=SOL-PERP'
```

Find the makers sitting at the top of one side of the book. `side` must be `bid` or `ask`:

```bash
curl 'http://127.0.0.1:6969/topMakers?marketName=SOL-PERP&side=bid&limit=5'
```

Fetch a user's resting CLOB orders. `marketIndexes` is an optional comma separated list. When it
is omitted, every perp market is read:

```bash
curl 'http://127.0.0.1:6969/userOrders?userPubkey=<PUBKEY>&marketIndexes=0,1'
```

Other routes are `/priorityFees` and `/batchPriorityFees` (both keyed by `marketType` and
`marketIndex`, served from the `DLOB_HELIUS` Redis prefix), `/unsettledPnlUsers`, and `/pythLazer`.

### `GET /marketOrderParams`

This route quotes the `OrderParams` a client signs for a perp market order. The required query
parameters are `marketIndex`, `direction` (`long` or `short`), `amount` and `assetType` (`base` or
`quote`). The optional ones are `slippageTolerance` (percent, dynamic when omitted),
`priceReference` (`best`, `mark`, `oracle` or `entry`, default `best`), `isOracleOrder`,
`activationDelaySlots`, `reduceOnly`, `userOrderId`, `maxLeverageSelected` and
`maxLeverageOrderSize`.

`data.params.price` is the worst price of the order. It is the reference price moved by the
tolerance, away from the taker. An oracle order carries it as `oraclePriceOffset` instead. The
response also carries the book-walk estimate (`entryPrice`, `bestPrice`, `worstPrice`,
`oraclePrice`, `markPrice`, `priceImpact`) and the tolerance it used.
[Trading the CLOB from a client](../../docs/clob-client-integration.md#market-orders) describes the
fields.

The dynamic tolerance reads `DYNAMIC_BASE_SLIPPAGE_*`, `DYNAMIC_SLIPPAGE_MULTIPLIER_*`,
`DYNAMIC_SLIPPAGE_MIN`, `DYNAMIC_SLIPPAGE_MAX`, `DYNAMIC_CROSS_SPREAD_CAP`,
`DYNAMIC_SLIPPAGE_WORST_PRICE_MARGIN` and `DYNAMIC_VAMM_QUOTE_MARGIN`. On a crossed book it also
reads the taker fill quality that `ENABLE_FILL_QUALITY_ANALYTICS` publishes.

## Websocket

Subscribe by sending a JSON message per channel after the socket opens. `channel` is `orderbook`,
`trades`, or `user_orders`. A market channel names the market in `market`. The `user_orders`
channel names a user pubkey in `user` instead, and streams that user's resting CLOB orders across
every market:

```json
{ "type": "subscribe", "marketType": "perp", "channel": "orderbook", "market": "SOL-PERP" }
{ "type": "subscribe", "marketType": "spot", "channel": "trades", "market": "SOL" }
{ "type": "subscribe", "channel": "user_orders", "user": "<PUBKEY>" }
```

Send the same object with `"type": "unsubscribe"` to stop a stream. `example/wsClient.ts` is a
working client you can run directly with `ts-node`.

`example/client.ts` and `example/clientWithSlot.ts` (the `example` and `exampleWithSlot` scripts)
fetch `/orders/idl`, an endpoint this server no longer exposes, so they do not run against the
current build.

# Scripts

`scripts/check-secrets.sh` scans staged files for RPC URLs, API keys, private keys and similar
patterns, and exits non-zero on a match. The repo's [Husky](https://typicode.github.io/husky/)
`pre-commit` hook is installed by `bun install` at the repo root through the `prepare` script, but
it currently returns early and does not run this scan, so run it yourself before committing:

```bash
bash scripts/check-secrets.sh
```

Pass `--no-verify` to `git commit` to skip Husky hooks entirely.
