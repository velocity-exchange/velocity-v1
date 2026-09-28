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

Set `PYTH_LAZER_TOKEN` to run the Pyth Lazer cranker and the mark TWAP crank. Without it the oracles
keep the prices in the dump, and an action that needs a fresh oracle fails once the price is older
than the market allows. `DEVNET_RPC_URL` picks the RPC the dump reads from.

Check the stack with a trade. The smoke test rests a CLOB bid, waits for it in `/userOrders`, and
cancels it with the handle the feed returns:

```bash
docker compose -f local-stack/compose.yaml run --rm --no-deps bootstrap bun run local-stack/smoke.ts
```

## What runs

| Service          | Port       | Role                                                            |
| ---------------- | ---------- | --------------------------------------------------------------- |
| `snapshot`       |            | Dumps devnet into the `state` volume on the first run only      |
| `validator`      | 8899, 8900 | `solana-test-validator` at the dump's slot                      |
| `rpc`            |            | Serves HTTP and websocket RPC on 8899 and on 8900               |
| `bootstrap`      |            | Keys, `migrate.ts`, CLOB books, crank treasury, configs         |
| `redis`          |            | One-node cluster. The TypeScript client has no plain mode       |
| `book-publisher` |            | Books, the user-orders feed, and the cross fast path            |
| `dlob-server`    | 6969       | HTTP: `/l2`, `/batchL2`, `/userOrders`, `/marketOrderParams`    |
| `dlob-ws`        | 3000       | Websocket: `orderbook`, `user_orders`                           |
| `relay-turner`   |            | Triggers, liquidations, expiry and crosses                      |
| `keepers`        |            | Funding, PnL settlement, mark TWAP, and Pyth Lazer when enabled |
| `swift`          | 3003       | Signed-message orders                                           |

The bootstrap writes `ui.env.local` into the `state` volume. It holds the overrides that point the
webapp at these ports:

```bash
docker compose -f local-stack/compose.yaml cp bootstrap:/state/ui.env.local ./ui.env.local
```

Services reach the validator through `rpc`. velocity-rs derives a websocket URL by changing only
the scheme, and web3.js moves it to the next port, so each port has to answer both.

Known gaps:

- `/batchPriorityFees` returns nothing, because its publisher calls a Helius-only RPC method.
- `sync_user_conditions` runs out of velocity's 32 KB heap for a user exposed in three book
  markets. `migrate.ts` stops at the first such user, so the users after it have no relay
  liquidation coverage here. The bootstrap reports this and continues.

## Reset

- `bun run local:reset` deletes the ledger. The next `up` boots from the same dump and runs the
  bootstrap again.
- `bun run local:resnapshot` deletes the dump and the keys too. The next `up` dumps devnet again.

The admin and hot-role keys in the dumped `State` are replaced with a local key at
`/state/snapshot/authority.json`. It signs every admin instruction, and the validator gives it the
genesis SOL.
