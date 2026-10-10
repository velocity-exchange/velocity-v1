#!/usr/bin/env bash
# Fails if a velocity .so contains an instruction that only devnet builds may carry.
#
# The mainnet flavor is the default feature set. A build line that adds
# --no-default-features compiles the devnet-only instructions back in, with no
# error. Anchor logs each instruction name, so the names are in the artifact.
#
# Usage: assert-mainnet-flavor.sh <velocity.so>
set -euo pipefail

DEVNET_ONLY_INSTRUCTIONS=(ForceWipeAccountsDevnet ExtendAccountDevnet)

if [ $# -ne 1 ] || [ ! -f "$1" ]; then
  echo "usage: assert-mainnet-flavor.sh <velocity.so>" >&2
  exit 2
fi

status=0
for name in "${DEVNET_ONLY_INSTRUCTIONS[@]}"; do
  if LC_ALL=C grep -aq "Instruction: ${name}" "$1"; then
    echo "ERROR: $1 contains the devnet-only instruction ${name}" >&2
    status=1
  fi
done

if [ $status -eq 0 ]; then
  echo "  ok: $1 has no devnet-only instructions"
fi
exit $status
