# swift-smoke-test

End-to-end smoke test for the Velocity Swift order flow on devnet (or mainnet).

Bootstraps a maker subscriber on SOL-PERP and places a taker oracle order via
the swift HTTP endpoint. Verifies WS connectivity, order acceptance, and that
the maker receives and attempts a fill transaction.

## Prerequisites

Two sub-accounts under the same keypair must exist on the target network:

- **Sub-account 0** — taker (needs USDC collateral for margin)
- **Sub-account 1** — maker (needs USDC collateral for margin)

Both need SOL for transaction fees. Use `init-devnet.sh` or the admin CLI to
create sub-accounts if they don't exist yet.

## Usage

```bash
cd rust/velocity-rs/examples/swift-smoke-test

# Devnet (default)
PRIVATE_KEY=<base58-keypair> cargo run

# Custom RPC
PRIVATE_KEY=<kp> RPC_URL=https://your-triton-rpc.com cargo run

# Mainnet
PRIVATE_KEY=<kp> MAINNET=1 RPC_URL=https://your-mainnet-rpc.com cargo run

# Verbose logs
RUST_LOG=info PRIVATE_KEY=<kp> cargo run
```

## Environment variables

| Variable    | Required | Default                          | Description                                     |
| ----------- | -------- | -------------------------------- | ----------------------------------------------- |
| `PRIVATE_KEY` | yes    | —                                | Base58 keypair (taker = sub-0, maker = sub-1)   |
| `RPC_URL`   | no       | `https://api.devnet.solana.com`  | Solana RPC endpoint                             |
| `MAINNET`   | no       | unset (devnet)                   | Set to any value to use mainnet swift endpoint  |
| `SWIFT_URL` | no       | devnet/mainnet default           | Override swift WS/HTTP base URL                 |

You can also drop a `.env` file in this directory and it will be loaded automatically.

## Expected output

```
=== Velocity Swift Smoke Test ===
context:  DevNet
taker:    <pubkey>
maker:    <pubkey> (sub-account 1)
market:   SOL-PERP (index 0)

[1/3] Starting maker subscriber...
  ✓ subscribed to sol-perp swift orders

[2/3] Placing taker swift order on SOL-PERP...
  posting to https://master.swift.drift.trade/orders
  ✓ order accepted (200): ...

[3/3] Waiting up to 20s for maker to fill...
  ✓ maker received order: <uuid>
  ✓ fill tx: <signature>

=== done ===
```

If the maker times out, the most likely causes are:
- Sub-account 1 has no USDC collateral (can't open a position)
- The devnet swift server isn't relaying orders to your subscription
- The taker order expired before the maker could fill (increase `AUCTION_SLOTS` in `main.rs`)
