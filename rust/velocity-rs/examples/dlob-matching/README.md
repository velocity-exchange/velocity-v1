# DLOB L3 methods example

Demonstrates the four L3 order retrieval methods with an oracle price you supply:
`get_maker_bids_l3`, `get_maker_asks_l3`, `get_taker_bids_l3`, and `get_taker_asks_l3`.

The example syncs user accounts with orders, builds a DLOB, keeps it live over gRPC, then
reads the current oracle price for perp market 0 and prints the top five orders from each
of the four methods.

The oracle price matters because floating limit orders price at an offset from it, oracle
orders track it, and trigger orders evaluate against it. Pass a stale or wrong price and
every one of those orders comes back at the wrong level.

## Run

```shell
RPC_URL=<https rpc> \
GRPC_URL=<grpc url> \
GRPC_X_TOKEN=<grpc token> \
cargo run --release
```

`GRPC_URL` and `GRPC_X_TOKEN` are required. `RPC_URL` defaults to
`https://api.mainnet-beta.solana.com`. Alternatively use a `.env` file.
