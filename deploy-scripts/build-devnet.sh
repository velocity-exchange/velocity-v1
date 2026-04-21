#!/bin/sh
# Builds both the drift program (no default features, no mainnet-beta gate)
# and the token_faucet program used to distribute devnet USDT.
set -eu
anchor build -p drift -- --no-default-features --features no-entrypoint
anchor build -p token_faucet