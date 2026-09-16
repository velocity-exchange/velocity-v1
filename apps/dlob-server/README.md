<div align="center">
  <img height="120" src="https://docs.velocity.exchange/assets/velocity.svg" />

  <h1>DLOB server</h1>

  <p>
    <a href="https://docs.velocity.exchange/developers/ecosystem-builders/orderbook-and-ws"><img alt="Docs" src="https://img.shields.io/badge/docs-developers-blueviolet" /></a>
    <a href="https://discord.com/invite/95kByNnDy5"><img alt="Discord Chat" src="https://img.shields.io/discord/849494028176588802?color=blueviolet" /></a>
    <a href="https://opensource.org/licenses/Apache-2.0"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blueviolet" /></a>
  </p>
</div>

This service reads the Velocity [DLOB](https://docs.velocity.exchange/protocol/how-it-works/orderbook-and-keepers)
from a Solana RPC node and serves it to clients. It runs in two modes that share the same codebase.

In HTTP mode, `src/index.ts` keeps a `VelocityClient` subscribed to perp and spot markets, builds
L2 and L3 order books from a DLOB source (websocket account subscriptions, gRPC, or the SDK's
`OrderSubscriber`), and answers REST requests. Most responses are read from Redis when a warm entry
exists and are rebuilt from the in-memory DLOB when it does not.

In websocket mode, `src/publishers/dlobPublisher.ts` snapshots the DLOB on an interval and writes
each snapshot to Redis, and `src/wsConnectionManager.ts` accepts client connections and relays the
latest snapshot for the channels a client subscribed to. The two processes talk only through Redis
pub/sub, so you can scale them independently.

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
| `USE_WEBSOCKET`                       | Set `true` to source the DLOB from websocket account subscriptions.              | `false`            |
| `USE_GRPC`                            | Set `true` to source the DLOB from a Yellowstone gRPC stream.                    | `false`            |
| `GRPC_ENDPOINT`                       | gRPC endpoint, used when `USE_GRPC` is set.                                      | `ENDPOINT/$TOKEN`  |
| `TOKEN`                               | gRPC auth token appended to `ENDPOINT` when `GRPC_ENDPOINT` is unset.            | none               |
| `USE_ORDER_SUBSCRIBER`                | Set `true` to source the DLOB from the SDK `OrderSubscriber`.                    | `false`            |
| `DISABLE_GPA_REFRESH`                 | Set `true` to stop periodically refreshing user accounts via `getProgramAccounts`. | `false`          |
| `ORDERBOOK_UPDATE_INTERVAL`           | Milliseconds between DLOB snapshots in the publisher.                            | `400`              |
| `PERP_MARKETS_TO_LOAD`                | Comma separated perp market indexes to load. Omit to load all.                   | all                |
| `SPOT_MARKETS_TO_LOAD`                | Comma separated spot market indexes to load. Omit to load all.                   | all                |
| `ELASTICACHE_HOST`                    | Redis host. In cluster mode this is the seed node and the rest are discovered.   | `localhost`        |
| `ELASTICACHE_PORT`                    | Redis port.                                                                      | `6379`             |
| `REDIS_CLIENT`                        | Comma separated key prefixes to use, from `DLOB` and `DLOB_HELIUS`.              | see below          |
| `LOCAL_CACHE`                         | Set `true` alongside `RUNNING_LOCAL` to talk to a local Redis cluster.           | unset              |
| `RUNNING_LOCAL`                       | Set `true` when Redis runs on the same machine.                                  | unset              |
| `WS_PORT`                             | Port the websocket connection manager listens on.                                | `3000`             |

When `REDIS_CLIENT` is unset the HTTP server and the connection manager open clients for both
`DLOB` and `DLOB_HELIUS`, while the publisher falls back to `DLOB` alone.

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

In the second terminal, run the publisher:

```bash
bun run dlob-publish
```

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

Fetch an aggregated L2 book. `depth` is clamped to 100 and defaults to 100. Set
`includeIndicative=true` to include indicative orders:

```bash
curl 'http://127.0.0.1:6969/l2?marketName=SOL-PERP&depth=10'
curl 'http://127.0.0.1:6969/l2?marketType=perp&marketIndex=0&depth=10&includeIndicative=true'
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

Other routes are `/priorityFees` and `/batchPriorityFees` (both keyed by `marketType` and
`marketIndex`, served from the `DLOB_HELIUS` Redis prefix), `/unsettledPnlUsers`, `/pythLazer`,
and `/auctionParams`, which requires `marketIndex`, `marketType`, `direction`, `amount` and
`assetType`.

## Websocket

Subscribe by sending a JSON message per channel after the socket opens. `channel` is `orderbook`
or `trades`, and `market` is the market name:

```json
{ "type": "subscribe", "marketType": "perp", "channel": "orderbook", "market": "SOL-PERP" }
{ "type": "subscribe", "marketType": "spot", "channel": "trades", "market": "SOL" }
```

Send the same object with `"type": "unsubscribe"` to stop a stream. `example/wsClient.ts` is a
working client you can run directly with `ts-node`.

`example/client.ts` and `example/clientWithSlot.ts` (the `example` and `exampleWithSlot` scripts)
fetch `/orders/idl`, an endpoint this server no longer exposes, so they do not run against the
current build.

# TOB monitoring

The publisher watches the top of book for a set of perp markets and forces the `OrderSubscriber` to
resubscribe when the book goes stale. Ghost orders that linger at the top of book are usually a
symptom of dropped account updates, which gRPC streams are prone to.

Monitoring is active only when all three of these hold: `ENABLE_TOB_MONITORING` is on,
`USE_ORDER_SUBSCRIBER` is on, and at least one monitored market index is loaded by this node.

## Configuration

- `ENABLE_TOB_MONITORING=false` turns monitoring off. It is on by default, including when the
  variable is unset.
- `TOB_CHECK_INTERVAL=60000` sets how often the check runs, in milliseconds. Default 60 seconds.
- `TOB_STUCK_THRESHOLD=60000` sets how long the top of book may sit unchanged before the publisher
  resubscribes, in milliseconds. Default 60 seconds.
- `TOB_MONITORING_ENABLED_PERP_MARKETS=0,1,2` lists the perp market indexes to watch. Default
  `0,1,2`.

## How it works

On each interval the publisher reads the L3 book for every monitored market that this node loaded,
standardized to the market's `orderTickSize`. It identifies the best bid and best ask by maker
pubkey and order id, tracking each side independently so an empty side still counts as a state. If
either identifier changes, it records the time and moves on. If neither has changed for longer than
`TOB_STUCK_THRESHOLD`, it logs a warning, records the stall duration, and runs unsubscribe,
subscribe, then fetch on the `OrderSubscriber` to clear the stale state.

## Metrics

- `tob_resubscribe` counts resubscribe attempts, labeled by market index and by whether the
  attempt succeeded.
- `tob_stuck_duration` records how many seconds the top of book had been unchanged when a
  resubscribe fired.

# Scripts

`scripts/check-secrets.sh` scans staged files for RPC URLs, API keys, private keys and similar
patterns, and exits non-zero on a match. The repo's [Husky](https://typicode.github.io/husky/)
`pre-commit` hook is installed by `bun install` at the repo root through the `prepare` script, but
it currently returns early and does not run this scan, so run it yourself before committing:

```bash
bash scripts/check-secrets.sh
```

Pass `--no-verify` to `git commit` to skip Husky hooks entirely.
