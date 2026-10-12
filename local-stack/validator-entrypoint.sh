#!/usr/bin/env bash
# Start solana-test-validator from the devnet dump in $SNAPSHOT_DIR.
#
# A new ledger boots at the dump's slot, so `clock.slot - last_update_slot`
# stays positive for every account in the dump. Velocity loads at this
# checkout's build, which leaves the dumped accounts in the state `migrate.ts`
# expects after an upgrade. An existing ledger resumes as it is, and then
# validator-upgrade deploys each program whose binary in $PROGRAMS_DIR changed.
set -euo pipefail
# A restarted container keeps its filesystem, so the last run's ready marker must go.
rm -f /tmp/ready
SNAPSHOT_DIR="${SNAPSHOT_DIR:-/state/snapshot}"
LEDGER_DIR="${LEDGER_DIR:-/ledger}"
PROGRAMS_DIR="${PROGRAMS_DIR:-/programs}"
AUTHORITY_KEYPAIR="${AUTHORITY_KEYPAIR:-$SNAPSHOT_DIR/authority.json}"
export LEDGER_DIR AUTHORITY_KEYPAIR

# Gossip refuses to advertise 0.0.0.0, and RPC binds where gossip does, so
# bind to the container's address. Other containers and the published ports
# both reach it there.
RPC_HOST="$(hostname -i | awk '{print $1}')"
ARGS=(--ledger "$LEDGER_DIR" --bind-address "$RPC_HOST" --rpc-port 8899 --log)
# By default the validator writes INFO metrics to a file in the ledger, about 3 GB a day.
# --log streams to stdout instead. A single validator has no epoch stakes, so
# cluster_slots warns on every slot.
export RUST_LOG="${RUST_LOG:-warn,solana_core::cluster_slots_service=error}"

# The marker appears once the validator first serves RPC. A ledger without it
# is what a failed first boot left, so it starts over.
FIRST_BOOT=false
if [ ! -e "$LEDGER_DIR/.booted" ]; then
  FIRST_BOOT=true
  find "$LEDGER_DIR" -mindepth 1 -delete
  AUTHORITY="$(solana-keygen pubkey "$AUTHORITY_KEYPAIR")"
  SLOT="$(grep -o '"slot": *[0-9]*' "$SNAPSHOT_DIR/manifest.json" | grep -o '[0-9]*$')"
  # With the default 432000-slot epoch, a warp to a devnet slot sets the clock about 13 hours
  # ahead of wall time, so every wall-clock expiry is already past on chain. A short epoch
  # re-anchors the clock to the validator's votes.
  ARGS+=(
    --warp-slot "$SLOT"
    --slots-per-epoch 432
    --mint "$AUTHORITY"
    --account-dir "$SNAPSHOT_DIR/accounts"
    --upgradeable-program vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR "$SNAPSHOT_DIR/vaults-devnet.so" "$AUTHORITY"
    --upgradeable-program V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB "$SNAPSHOT_DIR/token_faucet-devnet.so" "$AUTHORITY"
  )
  while read -r name id; do
    ARGS+=(--upgradeable-program "$id" "$PROGRAMS_DIR/$name.so" "$AUTHORITY")
  done < <(validator-upgrade --list)
fi

solana-test-validator "${ARGS[@]}" &
VALIDATOR_PID=$!
trap 'kill -TERM "$VALIDATOR_PID" 2>/dev/null' TERM INT

until curl -sf "http://$RPC_HOST:8899" -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | grep -q ok; do
  kill -0 "$VALIDATOR_PID" 2>/dev/null || wait "$VALIDATOR_PID"
  sleep 1
done
touch "$LEDGER_DIR/.booted"

# Genesis loaded the binaries in $PROGRAMS_DIR, so a first boot only records them.
if [ "$FIRST_BOOT" = true ]; then
  validator-upgrade --record-all
else
  validator-upgrade --sync
fi
touch /tmp/ready

wait "$VALIDATOR_PID"
