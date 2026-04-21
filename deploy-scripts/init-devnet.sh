#!/bin/sh
# Runs the devnet init runbook. Required env:
#   DEVNET_ADMIN       path to admin keypair json
#   USDT_MINT          devnet USDT SPL mint pubkey
#   SOL_LAZER_FEED_ID  pyth lazer u32 feed id for SOL/USD
# Optional: RPC_URL, LP_POOL_ID, LP_MAX_AUM, PROTECTED_MAKER_MAX_USERS, RECEIPT_PATH

set -eu

: "${DEVNET_ADMIN:?DEVNET_ADMIN must be set}"
: "${USDT_MINT:?USDT_MINT must be set}"
: "${SOL_LAZER_FEED_ID:?SOL_LAZER_FEED_ID must be set}"

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

cd "$repo_root"
exec bun run "$script_dir/init-devnet.ts"
