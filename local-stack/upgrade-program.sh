#!/usr/bin/env bash
# Build one program and upgrade it in place on the local stack or the sandbox, keeping the ledger.
#
#   upgrade-program.sh <local|sandbox> <velocity|clob|midpoint|relay> [--no-build]
#
# The deploy runs inside the validator container, so the buffer writes never cross the network.
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET="${1:?usage: upgrade-program.sh <local|sandbox> <program> [--no-build]}"
PROGRAM="${2:?usage: upgrade-program.sh <local|sandbox> <program> [--no-build]}"
BUILD=true
[ "${3:-}" = --no-build ] && BUILD=false

case "$PROGRAM" in
  velocity)
    [ "$BUILD" = true ] && bash deploy-scripts/build-sbf.sh devnet velocity
    SO=target/deploy/velocity.so
    ;;
  clob|midpoint)
    [ "$BUILD" = true ] && bash deploy-scripts/build-sbf-anchor-v2.sh "$PROGRAM"
    SO="anchor-v2/target/deploy/$PROGRAM.so"
    ;;
  relay)
    source test-scripts/_localnet.sh
    checkout_pinned_relay
    [ "$BUILD" = true ] && (cd "$RELAY_SRC/programs" && cargo-build-sbf --arch v3 --tools-version v1.57 --manifest-path relay/Cargo.toml)
    SO="$RELAY_SRC/programs/target/deploy/relay.so"
    ;;
  *)
    echo "unknown program '$PROGRAM'. Programs: velocity clob midpoint relay" >&2
    exit 1
    ;;
esac

case "$TARGET" in
  local)
    COMPOSE=(env DOCKER_DEFAULT_PLATFORM= docker compose -f local-stack/compose.yaml)
    "${COMPOSE[@]}" cp "$SO" "validator:/tmp/$PROGRAM.so"
    "${COMPOSE[@]}" exec validator validator-upgrade "$PROGRAM" "/tmp/$PROGRAM.so"
    ;;
  sandbox)
    POD=(-n "${SANDBOX_NAMESPACE:-pamm-sandbox}")
    kubectl "${POD[@]}" cp "$SO" "validator-0:/tmp/$PROGRAM.so" -c validator
    kubectl "${POD[@]}" exec validator-0 -c validator -- validator-upgrade "$PROGRAM" "/tmp/$PROGRAM.so"
    ;;
  *)
    echo "unknown target '$TARGET'. Targets: local sandbox" >&2
    exit 1
    ;;
esac
