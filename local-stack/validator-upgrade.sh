#!/usr/bin/env bash
# allow-verbose: the usage header of an operator command.
#
# Upgrade the stack's programs in place on the running validator.
#
#   validator-upgrade <program> <file.so>   deploy one binary now
#   validator-upgrade --sync                deploy each $PROGRAMS_DIR binary that changed
#   validator-upgrade --record-all          record the $PROGRAMS_DIR binaries as deployed
#   validator-upgrade --list                print each program's name and id
#
# --sync compares each binary with the hash it recorded under $LEDGER_DIR, not with the chain.
# A binary deployed by hand therefore stays until a different binary reaches $PROGRAMS_DIR.
set -euo pipefail
LEDGER_DIR="${LEDGER_DIR:-/ledger}"
PROGRAMS_DIR="${PROGRAMS_DIR:-/programs}"
AUTHORITY_KEYPAIR="${AUTHORITY_KEYPAIR:-/state/snapshot/authority.json}"
DEPLOYED_DIR="$LEDGER_DIR/.deployed"

PROGRAMS="velocity vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P
clob BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU
midpoint eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D
relay 4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"

program_id() {
  local id
  id="$(awk -v name="$1" '$1 == name { print $2 }' <<<"$PROGRAMS")"
  [ -n "$id" ] || { echo "unknown program '$1'. Programs: $(awk '{ printf "%s ", $1 }' <<<"$PROGRAMS")" >&2; exit 1; }
  echo "$id"
}

deploy() {
  local name="$1" file="$2"
  echo "== upgrading $name from $file =="
  solana program deploy \
    --url "http://$(hostname -i | awk '{print $1}'):8899" \
    --keypair "$AUTHORITY_KEYPAIR" \
    --upgrade-authority "$AUTHORITY_KEYPAIR" \
    --program-id "$(program_id "$name")" \
    "$file"
}

record() {
  mkdir -p "$DEPLOYED_DIR"
  sha256sum "$PROGRAMS_DIR/$1.so" | awk '{print $1}' >"$DEPLOYED_DIR/$1.sha256"
}

case "${1:-}" in
  --list)
    echo "$PROGRAMS"
    ;;
  --record-all)
    while read -r name _; do record "$name"; done <<<"$PROGRAMS"
    ;;
  --sync)
    while read -r name _; do
      marker="$DEPLOYED_DIR/$name.sha256"
      # A ledger from before the markers existed has nothing to compare against.
      if [ ! -e "$marker" ]; then
        record "$name"
        continue
      fi

      # A failed deploy keeps the old marker, so the next start tries again. The validator stays
      # up on the old binary instead of exiting.
      if [ "$(sha256sum "$PROGRAMS_DIR/$name.so" | awk '{print $1}')" != "$(cat "$marker")" ]; then
        if deploy "$name" "$PROGRAMS_DIR/$name.so"; then
          record "$name"
        else
          echo "== upgrade of $name FAILED; it still runs the previous binary ==" >&2
        fi
      fi
    done <<<"$PROGRAMS"
    ;;
  -*|'')
    sed -n '4,9p' "$0" >&2
    exit 1
    ;;
  *)
    deploy "$1" "${2:?usage: validator-upgrade <program> <file.so>}"
    ;;
esac
