#!/usr/bin/env bash
# allow-verbose: the usage header of an operator script.
#
# Rehearse the devnet upgrade against a copy of devnet's state on a local
# validator. Nothing is sent to devnet; the script only reads from it.
#
#   1. Dump devnet's velocity and vaults accounts (snapshot-devnet.ts) and the
#      deployed velocity and vaults binaries.
#   2. Boot solana-test-validator from that dump, at the dump's slot, with the
#      devnet binaries as upgradeable programs and CLOB, midpoint and relay as
#      new ones.
#   3. Upgrade velocity to this checkout's devnet build.
#   4. Run migrate.ts as a dry run, for real, then as a dry run again.
#   5. Run verify-upgrade.ts, which fails unless every account reads under the
#      new binary.
#
# Usage: bash test-scripts/rehearse-devnet-upgrade.sh [--skip-build] [--snapshot DIR] [--keep-running]
#   --snapshot DIR   reuse a dump, or write a new one there when DIR is empty
#   --keep-running   leave the validator up after the checks, for manual use
# DEVNET_RPC_URL names the cluster to dump (default the public devnet RPC).
# REHEARSAL_SCRATCH keeps the ledger and logs; a green run otherwise removes them.
set -euo pipefail
cd "$(dirname "$0")/.."

SKIP_BUILD=0
KEEP_RUNNING=0
SNAPSHOT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1 ;;
    --keep-running) KEEP_RUNNING=1 ;;
    --snapshot) SNAPSHOT="${2:?--snapshot needs a directory}"; shift ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
  shift
done

DEVNET_RPC_URL="${DEVNET_RPC_URL:-https://api.devnet.solana.com}"
RPC_PORT="${RPC_PORT:-8999}"
RPC_URL="http://127.0.0.1:$RPC_PORT"
VELOCITY_ID="vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P"
VAULTS_ID="vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR"
CLOB_ID="BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU"
MIDPOINT_ID="eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D"
RELAY_ID="4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"

if [ -n "${REHEARSAL_SCRATCH:-}" ]; then
  SCRATCH="$REHEARSAL_SCRATCH"
  SCRATCH_OWNED=0
  mkdir -p "$SCRATCH"
else
  SCRATCH="$(mktemp -d /tmp/velocity-rehearsal.XXXXXX)"
  SCRATCH_OWNED=1
fi
SNAPSHOT="${SNAPSHOT:-$SCRATCH/snapshot}"
export SDKROOT="${SDKROOT:-$(xcrun --show-sdk-path 2>/dev/null || true)}"

source test-scripts/_localnet.sh
resolve_test_validator
checkout_pinned_relay

if [ "$SKIP_BUILD" = 0 ]; then
  echo "== building the devnet flavor of velocity, CLOB, midpoint and relay =="
  bun run program:idl
  bash deploy-scripts/build-sbf.sh devnet velocity
  bun run program:build:clob
  bun run program:build:midpoint
  build_relay
  (cd packages/sdk && bun run build >/dev/null)
fi

RELAY_SO="$RELAY_SRC/programs/target/deploy/relay.so"
for f in target/deploy/velocity.so anchor-v2/target/deploy/clob.so anchor-v2/target/deploy/midpoint.so "$RELAY_SO"; do
  [ -e "$f" ] || { echo "missing $f — run without --skip-build" >&2; exit 1; }
done

mkdir -p "$SNAPSHOT"
if [ ! -e "$SNAPSHOT/manifest.json" ]; then
  echo "== dumping $DEVNET_RPC_URL into $SNAPSHOT =="
  bun run deploy-scripts/snapshot-devnet.ts --url "$DEVNET_RPC_URL" --out "$SNAPSHOT"
else
  echo "== reusing the dump in $SNAPSHOT =="
fi

for program in "velocity $VELOCITY_ID" "vaults $VAULTS_ID"; do
  set -- $program
  [ -e "$SNAPSHOT/$1-devnet.so" ] || solana program dump -u "$DEVNET_RPC_URL" "$2" "$SNAPSHOT/$1-devnet.so"
done

AUTHORITY="$SNAPSHOT/authority.json"
AUTHORITY_PUBKEY="$(solana-keygen pubkey "$AUTHORITY")"
DUMP_SLOT="$(bun -e "console.log(require('$SNAPSHOT/manifest.json').slot)")"

PIDS=()
GREEN=0
cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  if [ "$GREEN" = 1 ] && [ "$SCRATCH_OWNED" = 1 ]; then
    rm -rf "$SCRATCH"
  else
    echo "== scratch kept: $SCRATCH ==" >&2
  fi
}
trap cleanup EXIT INT TERM

free_ports "$RPC_PORT"

# The dump's accounts record devnet slots. Warping to the dump's slot keeps
# `clock.slot - last_update_slot` from going negative on the first crank.
echo "== starting solana-test-validator on :$RPC_PORT at slot $DUMP_SLOT =="
"$SOLANA_TEST_VALIDATOR" \
  --reset --quiet \
  --ledger "$SCRATCH/ledger" \
  --rpc-port "$RPC_PORT" \
  --warp-slot "$DUMP_SLOT" \
  --mint "$AUTHORITY_PUBKEY" \
  --account-dir "$SNAPSHOT/accounts" \
  --upgradeable-program "$VELOCITY_ID" "$SNAPSHOT/velocity-devnet.so" "$AUTHORITY_PUBKEY" \
  --upgradeable-program "$VAULTS_ID" "$SNAPSHOT/vaults-devnet.so" "$AUTHORITY_PUBKEY" \
  --bpf-program "$CLOB_ID" anchor-v2/target/deploy/clob.so \
  --bpf-program "$MIDPOINT_ID" anchor-v2/target/deploy/midpoint.so \
  --bpf-program "$RELAY_ID" "$RELAY_SO" \
  >"$SCRATCH/validator.log" 2>&1 &
PIDS+=($!)
wait_for_validator "$RPC_PORT" "$SCRATCH/validator.log"

echo "== upgrading velocity =="
solana program deploy target/deploy/velocity.so \
  --program-id "$VELOCITY_ID" \
  --upgrade-authority "$AUTHORITY" \
  --keypair "$AUTHORITY" \
  -u "$RPC_URL"

export RELAY_PROGRAM_ID="$RELAY_ID"
MIGRATE=(bun run deploy-scripts/migrate.ts --url "$RPC_URL" --keypair "$AUTHORITY")

echo "== migrate: dry run =="
"${MIGRATE[@]}" --dry-run | tee "$SCRATCH/migrate-plan.log"
echo "== migrate =="
"${MIGRATE[@]}" | tee "$SCRATCH/migrate.log"
echo "== migrate: dry run after =="
"${MIGRATE[@]}" --dry-run | tee "$SCRATCH/migrate-after.log"

echo "== verify =="
bun run deploy-scripts/verify-upgrade.ts --url "$RPC_URL" --keypair "$AUTHORITY"
echo "== rehearsal green =="
GREEN=1

if [ "$KEEP_RUNNING" = 1 ]; then
  echo "== validator at $RPC_URL, authority $AUTHORITY; Ctrl-C to stop =="
  wait "${PIDS[0]}"
fi
