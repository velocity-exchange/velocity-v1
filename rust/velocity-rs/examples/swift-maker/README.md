# Swift example maker

Listens to incoming swift orders on `sol-perp` and tries to fill them from the wallet's
default sub-account. It runs on devnet by default, where `RPC_URL` falls back to
`https://api.devnet.solana.com`. Edit the market list in `src/main.rs` to cover more
markets.

## Run

Run on devnet
```shell
PRIVATE_KEY="<base58 private key>" RUST_LOG=swift=debug cargo run --release
```

Run on mainnet
```shell
PRIVATE_KEY="<base58 private key>" MAINNET=1 RPC_URL="mainnet-rpc.example.com" RUST_LOG=swift=debug cargo run --release
```

Alternatively use a `.env` file.

## Logs

The latency number is the gap between the swift server accepting the taker order and this
client receiving it. Most of it is internal to the swift server, which simulates the tx for
correctness before forwarding it.

```log
uuid: F9gaynQV, latency: 74ms
```

The heartbeat message (ms) is the more accurate gauge of network latency.

```log
[2025-03-06T06:05:58Z DEBUG swift] heartbeat latency: 0
```
