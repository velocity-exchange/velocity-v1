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
[DLOB](https://docs.velocity.exchange/protocol/how-it-works/orderbook-and-keepers).

## Features

- Live DLOB data publishing
- Perp and spot markets
- WebSocket, gRPC, and polling subscriptions
- Health checks and metrics
- Top of book (TOB) monitoring for stuck orders. See
  [Top of book monitoring](#top-of-book-monitoring).

# Run the server

## Setup

Install dependencies from the repo root, then build this app:

```
bun install
bunx turbo run build --filter=@velocity-exchange/dlob-server
```

Set the environment variables:

```
cp .env.example .env
```

## Security

### Secret scan

`scripts/check-secrets.sh` scans the staged files for RPC URLs, API keys, tokens, and other
patterns that look like a secret. It exits non-zero on a match and names the file and the pattern.

Run it before a commit:

```bash
bash apps/dlob-server/scripts/check-secrets.sh
```

No hook runs it today. The repository pre-commit hook at `.husky/pre-commit` starts with `exit 0`,
so every check in it is skipped.

## Environment variables

Set these variables in your `.env` file:

| Variable                      | Description                                                     | Example Value                       |
| ----------------------------- | --------------------------------------------------------------- | ----------------------------------- |
| `ENDPOINT`                    | The Solana RPC node http endpoint.                              | `https://your-private-rpc-node.com` |
| `WS_ENDPOINT`                 | The Solana RPC node websocket endpoint.                         | `wss://your-private-rpc-node.com`   |
| `USE_WEBSOCKET`               | Flag to enable WebSocket connection.                            | `true`                              |
| `USE_ORDER_SUBSCRIBER`        | Flag to enable order subscriber DLOB source.                    | `true`                              |
| `DISABLE_GPA_REFRESH`         | Flag to disable periodic refresh using `getProgramAccounts`.    | `true`                              |
| `ENV`                         | The network environment the server is connecting to.            | `mainnet-beta`                      |
| `PORT`                        | The port number the HTTP server listens on.                     | `6969`                              |
| `METRICS_PORT`                | The port number for Prometheus metrics.                         | `9465`                              |
| `PRIVATE_KEY`                 | Path to the Solana private key file.                            | `/path/to/keypair.json`             |
| `RATE_LIMIT_CALLS_PER_SECOND` | Maximum number of API calls per second.                         | `100`                               |
| `PERP_MARKETS_TO_LOAD`        | Number of perpetual markets to load at startup.                 | `0`                                 |
| `SPOT_MARKETS_TO_LOAD`        | Number of spot markets to load at startup.                      | `5`                                 |
| `ELASTICACHE_HOST`            | (for websocket server) Redis host endpoint.                     | `localhost`                         |
| `ELASTICACHE_PORT`            | (for websocket server) Redis port.                              | `6379`                              |
| `REDIS_CLIENT`                | (for websocket server) Redis client type (DLOB/DLOB_HELIUS).    | `DLOB`                              |
| `WS_PORT`                     | (for websocket server) The port to run the websocket server on. | `3000`                              |

Note: to use several Redis hosts, give a comma separated string.

## HTTP mode

Start the HTTP server. It listens on `http://127.0.0.1:6969` by default. The
[orderbook and websocket docs](https://docs.velocity.exchange/developers/ecosystem-builders/orderbook-and-ws)
describe its routes.

```
bun run dev
```

## Websocket mode

The websocket server has two components. `dlob-publisher` takes frequent snapshots of the DLOB and
publishes them to Redis. `ws-manager` accepts new connections and sends the latest DLOB to websocket
clients. The two components talk through Redis pub-sub.

The websocket server needs a Redis cache and these environment variables:

- `REDIS_HOSTS`
- `REDIS_PASSWORDS`
- `REDIS_PORTS`

In the first terminal, start the redis cluster:

```
bash redisCluster.sh start
bash redisCluster.sh create
```

In a second terminal, run:

```
bun run dlob-publish
```

In a third terminal, run:

```
bun run ws-manager
```

Then connect to the websocket server at ws://127.0.0.1:3000.

To finish, stop the redis cluster:

```
bash redisCluster.sh stop
```

# Run the example client

`example/` holds three clients: `client.ts`, `clientWithSlot.ts`, and `wsClient.ts`. Run the first
two with `bun run example` and `bun run exampleWithSlot`. The
[orderbook and websocket docs](https://docs.velocity.exchange/developers/ecosystem-builders/orderbook-and-ws)
describe the protocol they speak.

## Top of book monitoring

`dlob-publisher` watches the top of book (TOB) of selected perp markets and detects a book that
stopped moving because of a stuck order. A gRPC connection can miss updates, which is the case this
catches.

### Configuration

- `ENABLE_TOB_MONITORING` - set to `true` to monitor. Monitoring is on unless the variable is set
  to something other than `true`.
- `TOB_CHECK_INTERVAL` - milliseconds between checks. Default `60000`.
- `TOB_STUCK_THRESHOLD` - milliseconds the TOB can stay unchanged before a resubscribe. Default
  `60000`.
- `TOB_MONITORING_ENABLED_PERP_MARKETS` - comma separated perp market indexes to monitor. Default
  `0,1,2`, which is SOL-PERP, BTC-PERP, and ETH-PERP.

Monitoring also needs `USE_ORDER_SUBSCRIBER`, because the recovery step drives the `OrderSubscriber`
instance. A node that loads none of the configured markets monitors nothing.

### How it works

1. Intersect `TOB_MONITORING_ENABLED_PERP_MARKETS` with the perp markets this node loaded.
2. Every `TOB_CHECK_INTERVAL`, read the best bid order id and the best ask order id of each of those
   markets.
3. When either id changed, record the time and stop.
4. When neither id changed for longer than `TOB_STUCK_THRESHOLD`, log a warning and recover.
5. Recovery calls `unsubscribe`, `subscribe`, and `fetch` on the `OrderSubscriber`, then resets the
   timer for that market.

### Metrics

- `tob_resubscribe` - counter of resubscribe attempts, labelled with `success`.
- `tob_stuck_duration` - gauge of the seconds the TOB stayed unchanged, per market.
