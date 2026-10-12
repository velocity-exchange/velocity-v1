#!/usr/bin/env bash
# Build one program and upgrade it in place on the local stack or the sandbox, keeping the ledger.
#
#   upgrade-program.sh <local|sandbox> <velocity|clob|midpoint|relay> [--no-build]
#
# Locally the deploy runs inside the validator container. The sandbox deploy reads the authority
# from the secret store and goes through the sandbox RPC host, so it needs Tailscale and an SSO
# session for the non-prod account.
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
    AUTHORITY="$(mktemp)"
    trap 'rm -f "$AUTHORITY"' EXIT
    aws secretsmanager get-secret-value --profile "${AWS_PROFILE:-velocity-non-prod}" --region eu-west-1 \
      --secret-id velocity/non-prod/pamm-sandbox-secret-store --query SecretString --output text |
      jq -r '."authority.json"' >"$AUTHORITY"
    PROGRAM_ID="$(bash local-stack/validator-upgrade.sh --list | awk -v name="$PROGRAM" '$1 == name { print $2 }')"
    solana program deploy --use-rpc \
      --url "${SANDBOX_RPC_URL:-https://pamm-sandbox-rpc.master.velocity.exchange}" \
      --keypair "$AUTHORITY" --upgrade-authority "$AUTHORITY" \
      --program-id "$PROGRAM_ID" "$SO"
    ;;
  *)
    echo "unknown target '$TARGET'. Targets: local sandbox" >&2
    exit 1
    ;;
esac
