#!/usr/bin/env bash
# Full-stack end-to-end harness against a real local validator — the devnet
# confidence gate. Stands up:
#   - solana-test-validator with velocity + pyth stub + CLOB + midpoint + relay
#   - redis-server (the book wire)
#   - the Rust book-publisher (simulated quote-view books + cross fast path)
#   - a relay crank-turner, running untrusted-mode against velocity
# then drives real trading through tests/e2e/localValidator.ts: protocol
# init, CLOB/midpoint/DLOB liquidity, router fills, place-and-take remainder
# resting, a publisher-detected cross match, Redis book assertions, and the
# relay-cranked flows (order expiry, trigger orders, liquidations) landing
# with nobody submitting them by hand.
#
# RELAY_REPO points at the relay checkout (default ~/source/relay); its
# program + turner are built from source, the same way velocity's are.
#
# Usage: bash test-scripts/run-e2e-localnet.sh [--skip-build]
#        E2E_SCRATCH=<dir> keeps the run's ledger and logs (a green run
#        otherwise removes the scratch directory it created).
# Requires: solana-test-validator, redis-server (brew install redis), bun.
set -euo pipefail
cd "$(dirname "$0")/.."

RPC_PORT="${RPC_PORT:-8899}"
REDIS_PORT="${REDIS_PORT:-6399}"
RELAY_REPO="${RELAY_REPO:-$HOME/source/relay}"
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

command -v solana-test-validator >/dev/null || { echo "solana-test-validator not on PATH" >&2; exit 1; }
command -v redis-server >/dev/null || { echo "redis-server not on PATH (brew install redis)" >&2; exit 1; }

VELOCITY_ID="vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P"
PYTH_ID="gSbePebfvPy7tRqimPoVecS2UsBvYv46ynrzWocc92s"
CLOB_ID="BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU"
MIDPOINT_ID="eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D"
RELAY_ID="4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"

[ -d "$RELAY_REPO" ] || { echo "relay checkout not found at $RELAY_REPO (set RELAY_REPO)" >&2; exit 1; }

if [ "${1:-}" != "--skip-build" ]; then
  echo "== building programs + publisher =="
  # Nothing here goes through `anchor build`, deliberately. Anchor always
  # uses cargo-build-sbf's default platform-tools (v1.52 as of 3.1.14) and
  # only exposes --solana-version for verifiable builds, while every .so this
  # harness deploys must come from v1.54 — the v1.52 default miscompiles
  # velocity's initialize paths into a 4KB stack-frame overflow ("Access
  # violation in stack frame 3"), which this very harness is what caught.
  # The two toolchains also cannot coexist: installing either removes the
  # other, so a run that used both would reinstall a toolchain every time
  # and, worse, would silently link whichever one happened to be present.
  # IDLs come from `program:idl`, which builds them with the host toolchain.
  bun run program:idl
  cargo-build-sbf --tools-version v1.54 --manifest-path programs/velocity/Cargo.toml -- --no-default-features --features no-entrypoint,anchor-test
  # The pyth stub rides velocity's build as a no-entrypoint library, so it
  # needs its own build WITH the entrypoint to be invocable on a validator.
  cargo-build-sbf --tools-version v1.54 --manifest-path programs/pyth/Cargo.toml -- --no-default-features --features anchor-test
  anchor idl build --skip-lint -p pyth -o target/idl/pyth.json -- --no-default-features --features anchor-test
  bun run program:build:clob
  bun run program:build:midpoint
  cargo build --manifest-path rust/Cargo.toml -p book-publisher
  cargo build --manifest-path rust/Cargo.toml -p swift-server
  # relay: the program the watches live on, and the turner that cranks them.
  (cd "$RELAY_REPO/programs" && cargo-build-sbf --tools-version v1.54 --manifest-path relay/Cargo.toml)
  cargo build --manifest-path "$RELAY_REPO/Cargo.toml" -p relay-crank-turner
  (cd packages/sdk && bun run build >/dev/null)
else
  for f in target/deploy/velocity.so target/deploy/pyth.so \
    anchor-v2/target/deploy/clob.so anchor-v2/target/deploy/midpoint.so \
    rust/target/debug/book-publisher \
    rust/target/debug/swift-server \
    "$RELAY_REPO/programs/target/deploy/relay.so" \
    "$RELAY_REPO/target/debug/relay-crank-turner"; do
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

# A validator orphaned by an earlier interrupted run keeps the port and its
# old ledger, and the suite then fails deep inside init with a confusing
# "already initialized". Clear the ports first, always.
for port in "$RPC_PORT" "$REDIS_PORT"; do
  pids=$(lsof -ti "tcp:$port" 2>/dev/null || true)
  if [ -n "$pids" ]; then
    echo "== port $port busy, killing $pids =="
    kill $pids 2>/dev/null || true
    sleep 2
  fi
done

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
  --bpf-program "$RELAY_ID" "$RELAY_REPO/programs/target/deploy/relay.so" \
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
export SWIFT_BIN="$PWD/rust/target/debug/swift-server"
export RELAY_TURNER_BIN="$RELAY_REPO/target/debug/relay-crank-turner"
export RELAY_PROGRAM_ID="$RELAY_ID"

echo "== running e2e suite (scratch: $SCRATCH) =="
PATH="$PWD/node_modules/.bin:$PATH" \
  ts-mocha -t 900000 ./tests/e2e/localValidator.ts
echo "== e2e suite green =="
SUITE_GREEN=1
