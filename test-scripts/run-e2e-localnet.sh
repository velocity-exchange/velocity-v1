#!/usr/bin/env bash
# Full-stack end-to-end harness against a real local validator — the devnet
# confidence gate. Stands up:
#   - solana-test-validator with velocity + pyth stub + CLOB + midpoint
#   - redis-server (the book wire)
#   - the Rust book-publisher (simulated quote-view books + cross fast path)
# then drives real trading through tests/e2e/localValidator.ts: protocol
# init, CLOB/midpoint/DLOB liquidity, router fills, place-and-take remainder
# resting, a publisher-detected cross match, and Redis book assertions.
#
# Usage: bash test-scripts/run-e2e-localnet.sh [--skip-build]
# Requires: solana-test-validator, redis-server (brew install redis), bun.
set -euo pipefail
cd "$(dirname "$0")/.."

RPC_PORT="${RPC_PORT:-8899}"
REDIS_PORT="${REDIS_PORT:-6399}"
SCRATCH="${E2E_SCRATCH:-$(mktemp -d /tmp/velocity-e2e.XXXXXX)}"
export SDKROOT="${SDKROOT:-$(xcrun --show-sdk-path 2>/dev/null || true)}"

command -v solana-test-validator >/dev/null || { echo "solana-test-validator not on PATH" >&2; exit 1; }
command -v redis-server >/dev/null || { echo "redis-server not on PATH (brew install redis)" >&2; exit 1; }

VELOCITY_ID="vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P"
PYTH_ID="gSbePebfvPy7tRqimPoVecS2UsBvYv46ynrzWocc92s"
CLOB_ID="BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU"
MIDPOINT_ID="eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D"

if [ "${1:-}" != "--skip-build" ]; then
  echo "== building programs + publisher =="
  bun run program:build
  # program:build compiles workspace programs with no-entrypoint (they ride
  # velocity's build as libraries); the pyth stub must be re-built WITH its
  # entrypoint to be invocable on the validator.
  anchor build --skip-lint --ignore-keys -p pyth -- --no-default-features --features anchor-test
  # Re-build velocity.so with platform-tools v1.54: the v1.52 default of
  # cargo-build-sbf 3.1.14 miscompiles initialize paths into a 4KB
  # stack-frame overflow ("Access violation in stack frame 3") — caught by
  # this very harness. See CLAUDE.md's toolchain notes.
  cargo-build-sbf --tools-version v1.54 --manifest-path programs/velocity/Cargo.toml -- --no-default-features --features no-entrypoint,anchor-test
  bun run program:build:clob
  bun run program:build:midpoint
  cargo build --manifest-path rust/Cargo.toml -p book-publisher
  (cd packages/sdk && bun run build >/dev/null)
else
  for f in target/deploy/velocity.so target/deploy/pyth.so \
    anchor-v2/target/deploy/clob.so anchor-v2/target/deploy/midpoint.so \
    rust/target/debug/book-publisher; do
    [ -e "$f" ] || { echo "missing $f — run without --skip-build" >&2; exit 1; }
  done
fi

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "== starting redis on :$REDIS_PORT =="
redis-server --port "$REDIS_PORT" --save '' --appendonly no \
  --dir "$SCRATCH" --logfile "$SCRATCH/redis.log" --daemonize no &
PIDS+=($!)

echo "== starting solana-test-validator on :$RPC_PORT =="
solana-test-validator \
  --reset --quiet \
  --ledger "$SCRATCH/ledger" \
  --rpc-port "$RPC_PORT" \
  --bpf-program "$VELOCITY_ID" target/deploy/velocity.so \
  --bpf-program "$PYTH_ID" target/deploy/pyth.so \
  --bpf-program "$CLOB_ID" anchor-v2/target/deploy/clob.so \
  --bpf-program "$MIDPOINT_ID" anchor-v2/target/deploy/midpoint.so \
  >"$SCRATCH/validator.log" 2>&1 &
PIDS+=($!)

echo "== waiting for validator health =="
for i in $(seq 1 60); do
  if curl -sf "http://127.0.0.1:$RPC_PORT" -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | grep -q '"ok"'; then
    break
  fi
  [ "$i" = 60 ] && { echo "validator never became healthy — see $SCRATCH/validator.log" >&2; exit 1; }
  sleep 1
done

export E2E_RPC_URL="http://127.0.0.1:$RPC_PORT"
export E2E_REDIS_URL="redis://127.0.0.1:$REDIS_PORT"
export E2E_SCRATCH_DIR="$SCRATCH"
export BOOK_PUBLISHER_BIN="$PWD/rust/target/debug/book-publisher"

echo "== running e2e suite (scratch: $SCRATCH) =="
PATH="$PWD/node_modules/.bin:$PATH" \
  ts-mocha -t 900000 ./tests/e2e/localValidator.ts
echo "== e2e suite green =="
