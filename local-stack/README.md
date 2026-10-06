# Local stack

A local copy of devnet, upgraded to this checkout, with the services a client trades against. The
validator starts from a dump of devnet's velocity and vaults accounts, so markets, mints, oracles
and users are devnet's. The program ID is devnet's, so a client runs in its devnet mode with its
URLs pointed here.

## Run it

```bash
bun run local:build   # host: velocity (devnet flavor), CLOB, midpoint, relay
bun run local:up      # docker: dump, validator, bootstrap, services
bun run local:logs
```

The first `local:up` on an arm64 host compiles agave from source. That took eight minutes on an M-series Mac,
once. Anza publishes no Linux arm64 build, and the x86_64 build fails under Rosetta because agave
requires io_uring. An amd64 host downloads the release instead.

The `local:*` scripts clear `DOCKER_DEFAULT_PLATFORM`, so the validator and the Rust services build
for the host. A plain `docker compose` run with that variable set to `linux/amd64` builds a
validator that cannot start on an arm64 host. The TypeScript services always run as amd64, because
`@triton-one/yellowstone-grpc` ships no linux-arm64 binding and the SDK loads it on import.

No Pyth Lazer token is needed. The dump adds a local signer to the Pyth Lazer storage account, and
the `oracle` service signs an update for every devnet feed about every 400 ms. Each price holds at
its value in the dump until it is moved:

```bash
curl localhost:7070/prices
curl -X POST localhost:7070/price -d '{"market":"SOL-PERP","price":150}'
```

Pyth's own signer stays trusted, so a live Lazer cranker would overwrite the local prices, and the
keepers do not run one. Set `PYTH_LAZER_TOKEN` only to run the mark TWAP crank, which bundles a live
Lazer update into each crank. `DEVNET_RPC_URL` picks the RPC the dump reads from.

Check the stack with a trade. The smoke test rests a CLOB bid, waits for it in `/userOrders`, and
cancels it with the handle the feed returns:

```bash
docker compose -f local-stack/compose.yaml run --rm --no-deps bootstrap bun run local-stack/smoke.ts
```

## Test helpers

`local:fund` sends SOL from the stack authority and mints dUSDT from the faucet. The defaults are
10 SOL and 10000 dUSDT:

```bash
bun run local:fund <pubkey> [--sol <amount>] [--dusdt <amount>]
```

`local:trader` is a second party for crosses and maker fills. Its first command funds it and opens
its account. `--name` picks another trader:

```bash
bun run local:trader -- rest SOL-PERP ask 130 0.5 --count 3 --step 1
bun run local:trader -- orders SOL-PERP
bun run local:trader -- take SOL-PERP buy 0.1 --worst 130
bun run local:trader -- swift SOL-PERP buy 0.1 --worst 130
bun run local:trader -- cancel SOL-PERP
```

`rest` is post-only unless `--cross` is passed. The usage header of `trader.ts` lists every
command and flag.

Every book runs a one-slot speed bump, as `migrate.ts` creates it. A take that carries no swift
attestation therefore does not fill in its own transaction. It rests whole as a taker-origin order,
and the relay's cross cranks fill it. A swift order carries an attestation, so it can fill in the transaction that `swift-placer` sends.

## Fill scenarios

`local:scenario` checks how a buy on SOL-PERP splits across the PropAMMs, the book and the vAMM.
Each scenario arranges the market, buys as trader b, and decodes every fill leg. The check fails
when a source fills other than the scenario expects, when a source fills at another cost than its
published L2 depth, or when a source fills above a price where another source left depth.

```bash
bun run local:scenario list
bun run local:scenario run three-way                 # swift, with the market's PropAMM route
bun run local:scenario run three-way --path onchain  # rests behind the speed bump, relay crosses it
```

| Scenario       | Arrangement                                                     | Expected fill                          |
| -------------- | --------------------------------------------------------------- | -------------------------------------- |
| `rung-walk`    | Midpoint `a` at the oracle. The vAMM quotes far above.          | 1.5 from `a`, across both of its rungs |
| `partial-rest` | Midpoint `a` at the oracle. The vAMM is paused.                 | 2 from `a`, and 0.5 rests on the book  |
| `three-way`    | Midpoint `a` and a book ask from trader c, both near the vAMM   | `a`, 0.5 from the book, and the vAMM   |
| `two-propamms` | Midpoint `b` 20 bps above `a`                                   | 2 from `a`, then 0.5 from `b`          |
| `propamm-tie`  | Midpoints `a` and `b` at the same mid                           | 1.25 from each, pro rata               |

Run each scenario on both paths after a change to the router, the quoter slab, the swift placement
or the taker-origin cross. `arrange <name>` sets the market up without buying, and
`verify <name> --authority <wallet>` checks an order that another client placed after the arrange.
The webapp's `propamm-local` Playwright project uses the two to record each scenario through the
trade form:

```bash
cd ~/source/protocol-v2-mono-wt/local-dev/e2e-test
TEST_LOCAL_STACK=true BASE_URL=http://localhost:3001 SWIFT_SERVER_URL=http://localhost:3003 \
  VELOCITY_V1_PATH=~/source/velocity-v1 PW_BROWSER_CHANNEL=chrome bun run propamm:local
```

The videos land in `e2e-test/test-results/*/video.webm`. Playwright's own ffmpeg is unsigned on
Apple Silicon, and macOS kills it. Link a Homebrew ffmpeg into its place once:

```bash
ln -sf /opt/homebrew/bin/ffmpeg ~/Library/Caches/ms-playwright/ffmpeg-1010/ffmpeg-mac
```

A midpoint instance that a scenario leaves out quotes 10% above the oracle, so it stays approved
but fills nothing. `swift-placer` compiles every placement against its own address lookup table
and extends the table with any account that a placement adds. A placement routed to two PropAMMs
names more accounts than a transaction holds without one.

## What runs

| Service          | Port       | Role                                                                  |
| ---------------- | ---------- | --------------------------------------------------------------------- |
| `snapshot`       |            | Dumps devnet on the first run only, then re-lays out its `User`s      |
| `validator`      | 8899, 8900 | `solana-test-validator` at the dump's slot                            |
| `oracle`         | 7070       | Signs and posts every Pyth Lazer feed, and moves a price on request   |
| `rpc`            |            | Serves HTTP and websocket RPC on 8899 and on 8900                     |
| `bootstrap`      |            | Keys, `migrate.ts`, CLOB books, crank treasury, configs               |
| `redis`          |            | One-node cluster. The TypeScript client has no plain mode             |
| `book-publisher` |            | Books, the user-orders feed, and the cross fast path                  |
| `dlob-server`    | 6969       | HTTP: `/l2`, `/batchL2`, `/userOrders`, `/marketOrderParams`          |
| `dlob-ws`        | 3000       | Websocket: `orderbook`, `user_orders`                                 |
| `relay-turner`   |            | Triggers, liquidations, expiry and crosses                            |
| `liquidator`     |            | keep-rs liquidator over websockets, the keeper floor under relay      |
| `keepers`        |            | Funding and PnL settlement, and the mark TWAP with a Lazer token      |
| `swift`          | 3003       | Signed-message intake and `/attest`                                   |
| `swift-ws`       |            | Swift's order websocket, which keepers subscribe to                   |
| `swift-placer`   |            | Attests each swift order and places it, as keep-rs does in production |

The bootstrap writes `ui.env.local` into the `state` volume. It holds the overrides that point the
webapp at these ports:

```bash
docker compose -f local-stack/compose.yaml cp bootstrap:/state/ui.env.local ./ui.env.local
```

Services reach the validator through `rpc`. velocity-rs derives a websocket URL by changing only
the scheme, and web3.js moves it to the next port, so each port has to answer both.

Known gaps:

- `/batchPriorityFees` returns nothing, because its publisher calls a Helius-only RPC method.

## Reset

- `bun run local:reset` deletes the ledger. The next `up` boots from the same dump and runs the
  bootstrap again.
- `bun run local:resnapshot` deletes the dump and the keys too. The next `up` dumps devnet again.

The admin and hot-role keys in the dumped `State` are replaced with a local key at
`/state/snapshot/authority.json`. It signs every admin instruction, and the validator gives it the
genesis SOL.
