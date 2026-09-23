<div align="center">
  <img height="120x" src="https://uploads-ssl.webflow.com/611580035ad59b20437eb024/616f97a42f5637c4517d0193_Logo%20(1)%20(1).png" />

  <h1 style="margin-top:20px;">DLOB Server for Velocity Protocol v1</h1>

  <p>
    <a href="https://docs.velocity.exchange/developers/trading-automation/keeper-bots"><img alt="Docs" src="https://img.shields.io/badge/docs-developers-blueviolet" /></a>
    <a href="https://discord.com/invite/95kByNnDy5"><img alt="Discord Chat" src="https://img.shields.io/discord/849494028176588802?color=blueviolet" /></a>
    <a href="https://opensource.org/licenses/Apache-2.0"><img alt="License" src="https://img.shields.io/github/license/project-serum/anchor?color=blueviolet" /></a>
  </p>
</div>

# DLOB Server

The backend server that provides a REST API for the Velocity
[order book](https://docs.velocity.exchange/protocol/how-it-works/orderbook-and-keepers).
Book production lives in the rust `book-publisher`, which writes each market's book to
Redis. This server reads those keys and serves them; it holds no order state itself.

## Features

- Serves each perp market's book, published to Redis by the rust `book-publisher`
- REST endpoints and a websocket fan-out over the same Redis keys
- Health checks and metrics

# Run the server

## Setup

Install dependencies and build:

```
yarn install
yarn build
```

Set the necessary environment variables:

```
cp .env.example .env
```

## Security

### Pre-commit Hook

This repository uses [Husky](https://typicode.github.io/husky/) to manage Git hooks. A pre-commit hook automatically checks for potential secrets before each commit. The hook will:

- Scan for RPC URLs, API keys, tokens, and other potential secrets
- Prevent commits that contain suspicious patterns
- Provide helpful guidance if false positives are detected

The hook is automatically installed when you run `npm install` (via the `prepare` script). If you need to bypass it for a specific commit, use:

```bash
git commit --no-verify
```

### Scripts

The `scripts/` directory contains utility scripts:

- `check-secrets.sh` - Secret detection script used by the pre-commit hook

## Environment Variables

To properly configure the DLOB server, set the following environment variables in your `.env` file:

| Variable                      | Description                                                     | Example Value                       |
| ----------------------------- | --------------------------------------------------------------- | ----------------------------------- |
| `ENDPOINT`                    | The Solana RPC node http endpoint.                              | `https://your-private-rpc-node.com` |
| `WS_ENDPOINT`                 | The Solana RPC node websocket endpoint.                         | `wss://your-private-rpc-node.com`   |
| `ENV`                         | The network environment the server is connecting to.            | `mainnet-beta`                      |
| `PORT`                        | The port number the HTTP server listens on.                     | `6969`                              |
| `METRICS_PORT`                | The port number for Prometheus metrics.                         | `9465`                              |
| `PRIVATE_KEY`                 | Path to the Solana private key file.                            | `/path/to/keypair.json`             |
| `RATE_LIMIT_CALLS_PER_SECOND` | Maximum number of API calls per second.                         | `100`                               |
| `MAX_BOOK_SLOT_LAG`           | How far a published book may trail the chain before it counts as behind. | `150`                      |
| `BOOK_FRESHNESS_INTERVAL_MS`  | How often to sample the published books for staleness.          | `5000`                              |
| `ENABLE_FILL_QUALITY_ANALYTICS` | Poll Athena for taker fill quality, which `/marketOrderParams` reads on a crossed book. Needs Athena credentials. | `false` |
| `ELASTICACHE_HOST`            | (for websocket server) Redis host endpoint.                     | `localhost`                         |
| `ELASTICACHE_PORT`            | (for websocket server) Redis port.                              | `6379`                              |
| `REDIS_CLIENT`                | (for websocket server) Redis client type (DLOB/DLOB_HELIUS).    | `DLOB`                              |
| `WS_PORT`                     | (for websocket server) The port to run the websocket server on. | `3000`                              |

Note: multiple Redis hosts can be provided by providing a comma separated string.

## HTTP mode

Start the HTTP server. It listens on `http://127.0.0.1:6969` by default. The
[orderbook and websocket docs](https://docs.velocity.exchange/developers/ecosystem-builders/orderbook-and-ws)
describe its routes.

```
bun run dev
```

### `GET /marketOrderParams`

Quotes the `OrderParams` a client signs for a perp market order. Required query parameters are
`marketIndex`, `direction` (`long` or `short`), `amount` and `assetType` (`base` or `quote`).
Optional ones are `slippageTolerance` (percent, dynamic when omitted), `priceReference` (`best`,
`mark`, `oracle` or `entry`, default `best`), `isOracleOrder`, `activationDelaySlots`,
`reduceOnly`, `userOrderId`, `maxLeverageSelected` and `maxLeverageOrderSize`.

`data.params.price` is the order's worst price: the reference price moved by the tolerance, away
from the taker. An oracle order carries it as `oraclePriceOffset` instead. The response also
carries the book-walk estimate (`entryPrice`, `bestPrice`, `worstPrice`, `oraclePrice`,
`markPrice`, `priceImpact`) and the tolerance it used. [Trading the CLOB from a
client](../../docs/clob-client-integration.md#market-orders) describes the fields.

The dynamic tolerance reads `DYNAMIC_BASE_SLIPPAGE_*`, `DYNAMIC_SLIPPAGE_MULTIPLIER_*`,
`DYNAMIC_SLIPPAGE_MIN`, `DYNAMIC_SLIPPAGE_MAX`, `DYNAMIC_CROSS_SPREAD_CAP`,
`DYNAMIC_SLIPPAGE_WORST_PRICE_MARGIN` and `DYNAMIC_VAMM_QUOTE_MARGIN`.

## Websocket mode

The websocket server has 2 components. The rust `book-publisher` (`rust/book-publisher`)
quotes every source through velocity's router view and writes each market's book to
Redis, and `ws-manager` listens for new connections and sends the latest book to ws
clients. The two components communicate through Redis pub-sub, so this server holds no
order state of its own.

To run the websocket server, a Redis cache is required, and the following environment variables must be set:

- `REDIS_HOSTS`
- `REDIS_PASSWORDS`
- `REDIS_PORTS`

In the first terminal, start the redis cluster:

```
bash redisCluster.sh start
bash redisCluster.sh create
```

In second terminal, run the publisher. `rust/` is its own cargo workspace, which
the root one excludes, so it is reached by manifest path:

```
cargo run --manifest-path rust/Cargo.toml -p book-publisher
```

Its `REDIS_KEY_PREFIX` must equal the prefix the server's Redis client applies,
which is `dlob:` for `REDIS_CLIENT=DLOB`. The publisher defaults to no prefix, so
every read misses until it is set.

In a third terminal, run:

```
yarn run ws-manager
```

Then connect to the ws server at ws://127.0.0.1:3000

When you're done, stop the redis cluster:

```
bash redisCluster.sh stop
```

# Run the example client

`example/` holds three clients: `client.ts`, `clientWithSlot.ts`, and `wsClient.ts`. Run the first
two with `bun run example` and `bun run exampleWithSlot`. The
[orderbook and websocket docs](https://docs.velocity.exchange/developers/ecosystem-builders/orderbook-and-ws)
describe the protocol they speak.

