# swift-smoke-test

SOL-PERP trade generator and end-to-end smoke test for the Velocity order flow
on devnet (or mainnet). Uses **two wallets** — a taker and a maker — bootstraps
and funds both, exercises the Swift signed-order path once, then continuously
generates DLOB trades.

## Wallets

- **Taker** — `PRIVATE_KEY` (required). Must hold SOL: it funds its own setup
  and tops the maker up with SOL.
- **Maker** — resolved in order: `MAKER_PRIVATE_KEY` env → a persisted
  `maker-keypair.json` in the working dir → a freshly generated keypair (written
  to `maker-keypair.json` and reused on later runs).

Each wallet trades from its own sub-account 0, so they have independent
`user_stats` accounts (a single wallet's sub-0/sub-1 share `user_stats`, which
makes back-to-back init racey — two wallets avoid that).

`maker-keypair.json` and `.env` are git-ignored.

## What it does

**Setup (`SETUP=1`):**

1. The taker transfers SOL to the maker authority (up to `MAKER_SOL_LAMPORTS`,
   default 0.15 SOL) so the maker can pay rent + fees.
2. Each wallet creates its dUSDT (quote token) ATA, faucet-mints into it via the
   `token_faucet`, initializes sub-account 0 (if missing), and deposits dUSDT.
   Each step is confirmed before the next. Idempotent — re-running tops up.

**Swift (one-shot, skip with `SKIP_SWIFT=1`):** the maker subscribes to SOL-PERP
swift orders and the taker posts an oracle order to the swift HTTP endpoint;
verifies acceptance and a maker fill attempt.

**DLOB (looped):** every `INTERVAL_SECS` (default 30) the maker places a resting
post-only limit order and the taker crosses it with a `place_and_take` limit
order — both sides of the book (taker buys / maker sells, then taker sells /
maker buys, returning both accounts to flat). The maker rests one passive offset
from oracle and the taker prices through it, so the cross matches the maker, not
the AMM. Runs forever unless `ITERATIONS` is set.

## Usage

```bash
cd rust/velocity-rs/examples/swift-smoke-test

# First run: generate + fund the maker, set up both accounts, then loop trades.
# The taker (PRIVATE_KEY) must already hold SOL.
PRIVATE_KEY=<base58-taker> SETUP=1 cargo run

# Subsequent runs: maker-keypair.json is reused; just generate trades every 30s
PRIVATE_KEY=<taker> SKIP_SWIFT=1 cargo run

# Bring your own maker key
PRIVATE_KEY=<taker> MAKER_PRIVATE_KEY=<maker> SETUP=1 cargo run

# Custom interval / fixed number of rounds
PRIVATE_KEY=<taker> SKIP_SWIFT=1 INTERVAL_SECS=15 ITERATIONS=10 cargo run

# Custom RPC (recommended — public devnet rate-limits)
PRIVATE_KEY=<taker> RPC_URL=https://your-triton-rpc.com cargo run

# Verbose logs
RUST_LOG=info PRIVATE_KEY=<taker> cargo run
```

You can also drop a `.env` file in this directory; it is loaded automatically.

## Environment variables

| Variable             | Required | Default                         | Description                                         |
| -------------------- | -------- | ------------------------------- | --------------------------------------------------- |
| `PRIVATE_KEY`        | yes      | —                               | Base58 taker keypair                                |
| `MAKER_PRIVATE_KEY`  | no       | generated + persisted           | Base58 maker keypair                                |
| `RPC_URL`            | no       | `https://api.devnet.solana.com` | Solana RPC endpoint                                 |
| `MAINNET`            | no       | unset (devnet)                  | Use mainnet swift/rpc/faucet endpoints              |
| `SWIFT_URL`          | no       | devnet/mainnet default          | Override swift WS/HTTP base URL                     |
| `SETUP`              | no       | unset                           | Fund maker SOL + init + faucet-fund + deposit       |
| `SKIP_SWIFT`         | no       | unset                           | Skip the one-shot swift smoke                       |
| `INTERVAL_SECS`      | no       | `30`                            | Seconds between DLOB rounds                         |
| `ITERATIONS`         | no       | `0` (forever)                   | Number of DLOB rounds                               |
| `FAUCET_AMOUNT`      | no       | `1000000`                       | Whole dUSDT to faucet-mint per account during setup |
| `DEPOSIT_AMOUNT`     | no       | `100000`                        | Whole dUSDT to deposit per account during setup     |
| `MAKER_SOL_LAMPORTS` | no       | `150000000` (0.15 SOL)          | SOL the taker tops the maker up to during setup     |

## Expected output

```
=== Velocity SOL-PERP trade generator ===
context:   DevNet
taker:     <taker-authority>  (sub <taker-sub>)
maker:     <maker-authority>  (sub <maker-sub>)
interval:  30s   iterations: ∞
market:    SOL-PERP (index 0)

=== Setup: fund maker SOL + initialize accounts + fund dUSDT ===
  funding maker <maker> with 0.1500 SOL
  ✓ maker SOL top-up: <sig>
  ✓ taker faucet fund: <sig>
  ✓ taker init + deposit (100000 dUSDT): <sig>
  ✓ maker faucet fund: <sig>
  ✓ maker init + deposit (100000 dUSDT): <sig>

=== DLOB trade generation (both sides every 30s) ===

── round 1 ──
  oracle=$73.5492  tick=10  step=100000  base=100000000

  ── side: taker BUY / maker SELL ──
  maker Short@73.6227  taker Long@73.6963
  ✓ maker resting order: <sig>
  ✓ taker crossing fill: <sig>

  ── side: taker SELL / maker BUY ──
  maker Long@73.4756  taker Short@73.4021
  ✓ maker resting order: <sig>
  ✓ taker crossing fill: <sig>
  DLOB results: buy-side ✓ filled, sell-side ✓ filled

── round 2 ──
  ...
```

## Troubleshooting

If setup's faucet step fails: the `token_faucet` must be initialized over the
dUSDT mint on the target network (devnet init handles this). `SETUP` is intended
for devnet.

If a maker SOL top-up or deposit fails: the taker wallet is out of SOL.

If a DLOB side reports no fill:

- An account lacks dUSDT collateral for the 0.1 SOL position (run with `SETUP=1`)
- The maker order didn't confirm within the poll window (RPC lag); a private RPC
  via `RPC_URL` is more reliable than public devnet.
