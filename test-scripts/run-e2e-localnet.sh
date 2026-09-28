#!/usr/bin/env bash
# Full-stack end-to-end harness against a real local validator — the devnet
# confidence gate. Stands up:
#   - solana-test-validator with velocity + pyth stub + CLOB + midpoint + relay
#   - redis-server (the book wire)
#   - the Rust book-publisher (simulated quote-view books + cross fast path)
#   - a relay crank-turner, running untrusted-mode against velocity
# then drives real trading through tests/e2e/localValidator.ts: protocol
# init, CLOB and midpoint liquidity, router fills, place-and-take remainder
# resting, a publisher-detected cross match, Redis book assertions, and the
# relay-cranked flows (order expiry, trigger orders, liquidations) landing
# with nobody submitting them by hand.
#
# RELAY_REPO points at the relay checkout (default ~/source/relay). The relay
# program and turner are built from source, the same way velocity's are, but
# from the revision `programs/velocity/Cargo.toml` pins rather than from
# whatever the checkout has on HEAD. The harness reads that revision and puts
# it in a detached git worktree of its own, so it never moves the checkout and
# never builds a relay that disagrees with the relay-spec velocity compiled
# against. RELAY_WORKTREE names the directory; it is reused between runs to
# keep the cargo cache warm.
#
# Usage: bash test-scripts/run-e2e-localnet.sh [--skip-build]
#        E2E_SCRATCH=<dir> keeps the run's ledger and logs (a green run
#        otherwise removes the scratch directory it created).
# Requires: solana-test-validator at agave 4.2 or later (the pinned relay's
# turner signs transaction v1), redis-server (brew install redis), bun.
# SOLANA_TEST_VALIDATOR names a specific binary.
set -euo pipefail
cd "$(dirname "$0")/.."

RPC_PORT="${RPC_PORT:-8899}"
REDIS_PORT="${REDIS_PORT:-6399}"
# Scratch holds the validator ledger and every service's log — a few hundred
# MB per run. A caller that names the directory owns it and it is never
# removed; one this script made is removed on success and kept on failure,
# where it is the only record of what happened. Pass E2E_SCRATCH=<dir> to keep
# a green run's artifacts.
if [ -n "${E2E_SCRATCH:-}" ]; then
  SCRATCH="$E2E_SCRATCH"
  SCRATCH_OWNED=0
else
  SCRATCH="$(mktemp -d /tmp/velocity-e2e.XXXXXX)"
  SCRATCH_OWNED=1
fi
export SDKROOT="${SDKROOT:-$(xcrun --show-sdk-path 2>/dev/null || true)}"

source test-scripts/_localnet.sh
resolve_test_validator
command -v redis-server >/dev/null || { echo "redis-server not on PATH (brew install redis)" >&2; exit 1; }

VELOCITY_ID="vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P"
PYTH_ID="gSbePebfvPy7tRqimPoVecS2UsBvYv46ynrzWocc92s"
CLOB_ID="BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU"
MIDPOINT_ID="eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D"
RELAY_ID="4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"

checkout_pinned_relay

if [ "${1:-}" != "--skip-build" ]; then
  echo "== building programs + publisher =="
  # Nothing here goes through `anchor build`, because anchor uses
  # cargo-build-sbf's default platform-tools and cannot build SBPFv3. Every
  # .so this harness deploys is SBPFv3 from the platform-tools that
  # `deploy-scripts/build-sbf.sh` pins. Agave 4.2 activates SIMD-0500 at
  # genesis, so the validator refuses to run a v0 program. IDLs come from
  # `program:idl`, which builds them with the host toolchain.
  bun run program:idl
  bash deploy-scripts/build-sbf.sh test velocity
  # The pyth stub rides velocity's build as a no-entrypoint library, so it
  # needs its own build WITH the entrypoint to be invocable on a validator.
  cargo-build-sbf --arch v3 --tools-version v1.57 --manifest-path programs/pyth/Cargo.toml --sbf-out-dir target/deploy -- --no-default-features --features anchor-test
  anchor idl build --skip-lint -p pyth -o target/idl/pyth.json -- --no-default-features --features anchor-test
  bun run program:build:clob
  bun run program:build:midpoint
  cargo build --manifest-path rust/Cargo.toml -p book-publisher
  cargo build --manifest-path rust/Cargo.toml -p swift-server
  # relay: the program the watches live on, and the turner that cranks them.
  build_relay
  (cd packages/sdk && bun run build >/dev/null)
else
  for f in target/deploy/velocity.so target/deploy/pyth.so \
    anchor-v2/target/deploy/clob.so anchor-v2/target/deploy/midpoint.so \
    rust/target/debug/book-publisher \
    rust/target/debug/swift-server \
    "$RELAY_SRC/programs/target/deploy/relay.so" \
    "$RELAY_SRC/target/debug/relay-crank-turner"; do
    [ -e "$f" ] || { echo "missing $f — run without --skip-build" >&2; exit 1; }
  done
fi

PIDS=()
SUITE_GREEN=0
cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  if [ "$SUITE_GREEN" = 1 ] && [ "$SCRATCH_OWNED" = 1 ] && [ -n "$SCRATCH" ]; then
    rm -rf "$SCRATCH"
  else
    echo "== scratch kept: $SCRATCH ==" >&2
  fi
}
trap cleanup EXIT INT TERM

free_ports "$RPC_PORT" "$REDIS_PORT"

echo "== starting redis on :$REDIS_PORT =="
redis-server --port "$REDIS_PORT" --save '' --appendonly no \
  --dir "$SCRATCH" --logfile "$SCRATCH/redis.log" --daemonize no &
PIDS+=($!)

echo "== starting solana-test-validator on :$RPC_PORT =="
"$SOLANA_TEST_VALIDATOR" \
  --reset --quiet \
  --ledger "$SCRATCH/ledger" \
  --rpc-port "$RPC_PORT" \
  --bpf-program "$VELOCITY_ID" target/deploy/velocity.so \
  --bpf-program "$PYTH_ID" target/deploy/pyth.so \
  --bpf-program "$CLOB_ID" anchor-v2/target/deploy/clob.so \
  --bpf-program "$MIDPOINT_ID" anchor-v2/target/deploy/midpoint.so \
  --bpf-program "$RELAY_ID" "$RELAY_SRC/programs/target/deploy/relay.so" \
  >"$SCRATCH/validator.log" 2>&1 &
PIDS+=($!)

wait_for_validator "$RPC_PORT" "$SCRATCH/validator.log"

export E2E_RPC_URL="http://127.0.0.1:$RPC_PORT"
export E2E_REDIS_URL="redis://127.0.0.1:$REDIS_PORT"
export E2E_SCRATCH_DIR="$SCRATCH"
export BOOK_PUBLISHER_BIN="$PWD/rust/target/debug/book-publisher"
export SWIFT_BIN="$PWD/rust/target/debug/swift-server"
export RELAY_TURNER_BIN="$RELAY_SRC/target/debug/relay-crank-turner"
export RELAY_PROGRAM_ID="$RELAY_ID"

echo "== running e2e suite (scratch: $SCRATCH) =="
PATH="$PWD/node_modules/.bin:$PATH" \
  ts-mocha -t 900000 ./tests/e2e/localValidator.ts
echo "== e2e suite green =="
SUITE_GREEN=1
