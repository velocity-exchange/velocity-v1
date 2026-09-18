#!/bin/bash

set -e
trap 'echo -e "\nStopped by signal $? (SIGINT)"; exit 0' INT

export PATH="$PWD/bin:$PWD/node_modules/.bin:$PATH"

if [ "$1" != "--skip-build" ]; then
  anchor build --ignore-keys --skip-lint -- --features anchor-test && anchor test --skip-build --skip-local-validator --skip-deploy &&
    cp target/idl/velocity.json packages/sdk/src/idl/ && cp target/types/velocity.ts packages/sdk/src/idl/
else
  # --skip-build still needs the bundled SDK IDL to match the deployed program ID,
  # otherwise tx instructions target a program that bankrun never loaded. With the
  # CI program cache this dir is always populated (restored on a hit, freshly built
  # on a miss), so a missing IDL means the caller skipped the build by mistake — fail
  # loudly rather than silently testing against a stale bundled IDL.
  if [ ! -f target/idl/velocity.json ]; then
    echo "ERROR: target/idl/velocity.json is missing — cannot guarantee SDK IDL matches deployed program." >&2
    echo "       Run without --skip-build, or copy a fresh IDL into target/idl/ first." >&2
    exit 1
  fi
  if [ ! -f target/types/velocity.ts ]; then
    echo "ERROR: target/types/velocity.ts is missing — cannot guarantee SDK types match deployed program." >&2
    echo "       Run without --skip-build, or copy fresh types into target/types/ first." >&2
    exit 1
  fi
  # Same reasoning for the programs themselves, and this one fails far worse
  # without a guard: the bankrun harness loads every workspace program, and a
  # missing .so produces only a "Possible bogus program name" *warning* before
  # the affected file's `before all` hook hangs for its full 300s timeout with
  # no statement of the cause. Wiping target/deploy to clear stale SBF
  # artifacts is a routine thing to do, and it silently invalidates
  # --skip-build.
  # The list comes from Anchor.toml's [programs.localnet], which is what
  # solana-bankrun's startAnchor loads, so adding a program there cannot
  # silently escape this check.
  missing=()
  while read -r program; do
    [ -f "target/deploy/${program}.so" ] || missing+=("${program}.so")
  done < <(awk '/^\[programs\.localnet\]/{f=1;next} /^\[/{f=0} f && /=/{print $1}' Anchor.toml)
  if [ ${#missing[@]} -ne 0 ]; then
    echo "ERROR: target/deploy is missing: ${missing[*]}" >&2
    echo "       The bankrun harness loads every workspace program; a missing one" >&2
    echo "       hangs a test file's setup for 300s instead of failing." >&2
    echo "       Run without --skip-build, or build the missing ones with" >&2
    echo "       cargo-build-sbf --tools-version v1.54 --manifest-path programs/<name>/Cargo.toml" >&2
    exit 1
  fi

  cp target/idl/velocity.json packages/sdk/src/idl/
  cp target/types/velocity.ts packages/sdk/src/idl/
fi

# Build the SDK in both paths: many test files import the package root
# (`from '../packages/sdk'`), which resolves through package.json `main` to
# packages/sdk/lib/node/index.js. ts-mocha only transpiles the
# `../packages/sdk/src/...` imports on the fly, so without this build those
# bare-package imports fail with MODULE_NOT_FOUND in CI. Runs after the IDL is
# synced into src/idl/ above so lib/ reflects the freshly-built program.
( cd packages/sdk && bun run build >/dev/null )

export ANCHOR_WALLET=~/.config/solana/id.json

test_files=(
  # cappedSymFunding.ts
  # delistMarket.ts
  # delistMarketLiq.ts
  # imbalancePerpPnl.ts
  # ksolver.ts
  # repegAndSpread.ts
  # spotWithdrawUtil100.ts
  # updateAMM.ts
  # updateK.ts
  # TODO BROKEN ^^
  # A commented entry with no reason beside it needs a filled perp position,
  # and this harness cannot produce one. Every v1 order path routes: it names
  # the market's quoter slab, its book and the CLOB program, and it CPIs the
  # book to quote. solana-bankrun is pinned at 0.4.0, which is also the newest
  # published version, and its VM does not set r2 at program entry. The CLOB's
  # entrypoint reads the input length out of r2, so the book faults on its
  # first instruction with "Access violation in unknown section at address
  # 0xfffffffffffffff8". velocity.so, built by the same platform-tools v1.54,
  # runs there, so it is the entry ABI and not the toolchain.
  #
  # Routed order-flow coverage therefore lives in integration-tests/, which
  # runs on litesvm and loads the real CLOB and midpoint .so files. These files
  # are kept rather than deleted: they run again the moment a bankrun release
  # can execute the book.
  #
  # What moved there so far: the vAMM's own pricing, which had no end-to-end
  # cover anywhere once these stopped running. `a_vamm_take_pays_the_spread_
  # inside_its_entry_price` and `a_bid_between_the_mark_and_the_ask_takes_
  # nothing_from_the_vamm` pin the spread from both sides, and
  # `a_vamm_position_opens_reduces_reverses_and_closes` walks a position across
  # zero. Zeroing `base_spread` in the litesvm fixture fails both spread specs,
  # which is the check that they hold the behaviour rather than merely pass.
  #
  # Not yet moved: the prepeg and repeg paths (updateAMM, repegAndSpread,
  # prepegMarketOrderBaseAssetAmount) and the rounding direction
  # (roundInFavorBaseAsset). The repeg math itself is unit-tested under
  # programs/velocity/src/vlp/amm/; what is missing is its integration with a
  # fill.
  #
  # Also not yet moved, and larger: the liquidation suites (liquidatePerp,
  # liquidatePerpWithFill, liquidatePerpPnlForDeposit, liquidateBorrowForPerpPnl,
  # bankruptcyIfFloor), the margin and isolated-position suites, and the
  # protocol-fee, referrer, pause and delegate suites. Each opens a perp
  # position to reach what it measures, which is why it stopped running here.
  # integration-tests/ covers a liquidation through the book
  # (`a_liquidation_fills_through_the_book`) and the distress ladder, and the
  # rest is unit-tested under programs/velocity/src/; what is missing is the
  # end-to-end wiring these files held. Moving them is the outstanding work.
  #
  # An entry that carries its own reason is commented for that reason instead.
  # builderCodes.ts
  decodeUser.ts
  admin.ts
  accountExtension.ts
  equityFloorSwap.ts
  assetTier.ts
  # cancelAllOrders.ts
  # curve.ts
  deleteInitializedSpotMarket.ts
  depositIntoSpotMarketVault.ts
  # equityFloor.ts
  equityFloorSwap.ts
  equityFloorLazyTrip.ts
  equityFloorOracle.ts
  # equityFloorFillGates.ts
  # equityBreakerFreeze.ts
  # velocityClient.ts
  insuranceFundStake.ts
  # isolatedPositionVelocityClient.ts
  # isolatedPositionLiquidatePerp.ts
  # isolatedPositionLiquidatePerpwithFill.ts
  # bankruptcyIfFloor.ts
  # liquidateBorrowForPerpPnl.ts
  # liquidatePerp.ts
  # liquidatePerpWithFill.ts
  # liquidatePerpPnlForDeposit.ts
  liquidateSpot.ts
  liquidateSpotSocialLoss.ts
  liquidateSpotWithSwap.ts
  # lpPool.ts # depends on PerpMarket layout shift — needs re-snapshot
  # lpPoolSwap.ts # depends on PerpMarket layout shift — needs re-snapshot
  # marketOrder.ts
  # marketOrderBaseAssetAmount.ts
  maxDeposit.ts
  # maxLeverageOrderParams.ts
  mmOracleBatchNative.ts
  # modifyOrder.ts
  oracleDiffSources.ts
  # oracleFillPriceGuardrails.ts
  # order.ts
  # orderMarginChecks.ts
  # isolatedTransferMarginChecks.ts
  # ordersWithSpread.ts
  # pauseExchange.ts
  pauseDepositWithdraw.ts
  placeAndMakeSignedMsgBankrun.ts
  # prelisting.ts
  # pyth.ts
  pythLazerBankrun.ts
  # referrer.ts
  # roundInFavorBaseAsset.ts
  # settlePNLInvariant.ts
  spotDepositWithdraw.ts
  spotDepositWithdraw22.ts
  spotDepositWithdraw22TransferHooks.ts
  spotMarketPoolIds.ts
  # spotSwap.ts # broken by spot fulfillment purge — needs migration to read serum vaults directly off the Market
  # spotSwap22.ts # broken by spot fulfillment purge — needs migration to read serum vaults directly off the Market
  swapPostEndIxs.ts
  # stopLimits.ts
  subaccounts.ts
  surgePricing.ts
  switchOracle.ts
  # triggerOrders.ts
  # transferPerpPosition.ts
  # userAccount.ts
  # userDelegate.ts
  # perpMarketConfig.ts # market_config field reads as 0 after write — possibly fetch caching or layout mismatch with reordered PerpMarket

  # whitelist.ts
  transferFeeAndPnlPool.ts
  # protocolFees.ts
  recenterAmmCrankOracle.ts
  # specialUserAccount.ts
)

# Run up to PARALLEL tests concurrently. Output is buffered per test and only
# printed on failure so interleaved stdout from concurrent processes doesn't
# obscure which test failed.
PARALLEL=${PARALLEL:-4}
# Stop at the first failing file by default: a red suite is red, and the rest
# of the run costs minutes that tell nobody anything new. KEEP_GOING=1 runs
# every file anyway, which is what a census after a wide change needs.
KEEP_GOING=${KEEP_GOING:-0}
tmpdir=$(mktemp -d)
trap "rm -rf '$tmpdir'" EXIT

declare -a q_pids=()
declare -a q_files=()
declare -a q_logs=()
overall_failed=0

# Reap whichever queued child finishes first, to avoid head-of-line blocking
# when the oldest test is slow. We block until at least one child exits, then
# identify it by pid and `wait` that specific pid for its status — so the
# pass/fail label always matches the test that produced it, even when several
# finish in the same window.
#
# `wait -n` (bash >= 4.3) blocks efficiently; on bash 3.2 (macOS default) we
# fall back to a short kill -0 poll. `wait -n || :` keeps a non-zero exit from
# the reaped test out of `set -e`'s way; the per-pid `if wait` does the same.
collect_any() {
  if [ "${BASH_VERSINFO[0]}" -gt 4 ] || \
     { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -ge 3 ]; }; then
    wait -n || :
  fi

  # Find a finished child: one is guaranteed reaped after `wait -n`; on the
  # fallback path we spin (with a tiny sleep, no busy-wait) until one exits.
  local idx=-1
  while true; do
    local i
    for i in "${!q_pids[@]}"; do
      if ! kill -0 "${q_pids[$i]}" 2>/dev/null; then
        idx=$i
        break
      fi
    done
    [ $idx -ne -1 ] && break
    sleep 0.2
  done

  local pid="${q_pids[$idx]}"
  local file="${q_files[$idx]}"
  local log="${q_logs[$idx]}"
  # Remove the reaped entry from all three parallel arrays.
  q_pids=("${q_pids[@]:0:$idx}" "${q_pids[@]:$(( idx + 1 ))}")
  q_files=("${q_files[@]:0:$idx}" "${q_files[@]:$(( idx + 1 ))}")
  q_logs=("${q_logs[@]:0:$idx}" "${q_logs[@]:$(( idx + 1 ))}")
  # `wait <pid>` returns that child's remembered status even after it was
  # already reaped by `wait -n` or bash's async reaper.
  if wait "$pid"; then
    echo "  pass: $file"
  else
    echo ""
    echo "══════════════════════════════════════"
    echo "  FAIL: $file"
    echo "══════════════════════════════════════"
    cat "$log"
    overall_failed=1
  fi
}

for test_file in "${test_files[@]}"; do
  [ $overall_failed -eq 1 ] && [ "$KEEP_GOING" = 0 ] && break
  while [ ${#q_pids[@]} -ge $PARALLEL ]; do
    collect_any
    [ $overall_failed -eq 1 ] && [ "$KEEP_GOING" = 0 ] && break 2
  done
  log="$tmpdir/${test_file}"
  ts-mocha --exit -t 300000 "./tests/velocity/$test_file" >"$log" 2>&1 &
  q_pids+=($!)
  q_files+=("$test_file")
  q_logs+=("$log")
  echo "  start: $test_file"
done

while [ ${#q_pids[@]} -gt 0 ]; do
  collect_any
done

[ $overall_failed -eq 0 ] || exit 1
