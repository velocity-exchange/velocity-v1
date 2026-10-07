#!/usr/bin/env bash
# Start solana-test-validator from the devnet dump in /state/snapshot.
#
# A new ledger boots at the dump's slot, so `clock.slot - last_update_slot`
# stays positive for every account in the dump. Velocity loads at this
# checkout's build, which leaves the dumped accounts in the state `migrate.ts`
# expects after an upgrade. An existing ledger resumes as it is.
set -euo pipefail
SNAPSHOT=/state/snapshot
# Gossip refuses to advertise 0.0.0.0, and RPC binds where gossip does, so
# bind to the container's address. Other containers and the published ports
# both reach it there.
ARGS=(--ledger /ledger --bind-address "$(hostname -i | awk '{print $1}')" --rpc-port 8899 --quiet)

# The healthcheck writes the marker once the validator first serves RPC. A
# ledger without it is what a failed first boot left, so it starts over.
if [ ! -e /ledger/.booted ]; then
  find /ledger -mindepth 1 -delete
  AUTHORITY="$(solana-keygen pubkey "$SNAPSHOT/authority.json")"
  SLOT="$(grep -o '"slot": *[0-9]*' "$SNAPSHOT/manifest.json" | grep -o '[0-9]*$')"
  # With the default 432000-slot epoch, a warp to a devnet slot sets the clock about 13 hours
  # ahead of wall time, so every wall-clock expiry is already past on chain. A short epoch
  # re-anchors the clock to the validator's votes.
  ARGS+=(
    --warp-slot "$SLOT"
    --slots-per-epoch 432
    --mint "$AUTHORITY"
    --account-dir "$SNAPSHOT/accounts"
    --upgradeable-program vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P /programs/velocity.so "$AUTHORITY"
    --upgradeable-program vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR "$SNAPSHOT/vaults-devnet.so" "$AUTHORITY"
    --upgradeable-program V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB "$SNAPSHOT/token_faucet-devnet.so" "$AUTHORITY"
    --bpf-program BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU /programs/clob.so
    --bpf-program eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D /programs/midpoint.so
    --bpf-program 4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu /programs/relay.so
  )
fi

exec solana-test-validator "${ARGS[@]}"
