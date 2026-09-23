# Swift example taker

Example swift taker client. It signs a market-index 0 perp order and posts it to the swift
order server. It runs on devnet by default, where `RPC_URL` falls back to
`https://api.devnet.solana.com`.

## Run

Run on devnet
```shell
PRIVATE_KEY="<base58 private key>" RUST_LOG=swift=debug cargo run --release

# Deposit Trade
PRIVATE_KEY="<base58 private key>" RUST_LOG=swift=debug cargo run --release -- --deposit-trade

# Isolated Margin Order
PRIVATE_KEY="<base58 private key>" RUST_LOG=swift=debug cargo run --release -- --isolated-position 100000000

```

Run on mainnet
```shell
PRIVATE_KEY="<base58 private key>" MAINNET=1 RPC_URL="mainnet-rpc.example.com" RUST_LOG=swift=debug cargo run --release
```

Alternatively use a `.env` file.

## Flags

- `--deposit-trade`: makes a depositTrade request that deposits collateral and places an
  order in a single transaction
- `--isolated-position <integer>`: makes the order use isolated margin with the supplied
  amount of USDC collateral, in base units (100000000 = 100 USDC)
