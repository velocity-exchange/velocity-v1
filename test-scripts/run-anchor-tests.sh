#!/bin/bash

set -e
trap 'echo -e "\nStopped by signal $? (SIGINT)"; exit 0' INT

export PATH="$PWD/bin:$PWD/node_modules/.bin:$PATH"


usage() {
	cat <<USAGE
usage: $0 [options]

Run the velocity integration suite (tests/velocity/) under ts-mocha + LiteSVM.
Builds the SBF programs and regenerates the IDL first unless --skip-build.

options:
  --skip-build              reuse target/deploy/*.so and target/idl (they must
                            already exist; the suite fails loudly if not)
  --no-color                plain output (also honours NO_COLOR)
  -h, --help                this text

environment:
  SINGLE_PROCESS=1          run every file in one mocha process instead of one
                            process each: 53s -> 19s. Each file still gets its
                            own LiteSVM, but module-level state is shared, so
                            this is the local loop and not the CI gate yet.
  PARALLEL=<n>              concurrent files in the default per-file mode
                            (default 4; ignored when SINGLE_PROCESS=1)
  SVM_DEPLOY_DIR=<dir>      load programs from here instead of target/deploy

examples:
  $0 --skip-build
  SINGLE_PROCESS=1 $0 --skip-build
USAGE
}

skip_build=0
use_color=1
for arg in "$@"; do
	case "$arg" in
		--skip-build) skip_build=1 ;;
		--no-color)   use_color=0 ;;
		-h|--help)    usage; exit 0 ;;
		*) echo "unknown option: $arg" >&2; echo >&2; usage >&2; exit 2 ;;
	esac
done

# Same gate as deploy-scripts/_ui.sh so the whole toolchain agrees on when to
# emit escapes.
if [ "$use_color" -eq 1 ] && [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
	export TEST_REPORT_COLOR=1
	C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'; C_RED=$'\033[31m'
	C_GRN=$'\033[32m'; C_RST=$'\033[0m'
else
	export TEST_REPORT_COLOR=0
	C_BOLD=""; C_DIM=""; C_RED=""; C_GRN=""; C_RST=""
fi

# Progress for the setup steps, in the same shape the reporter uses for test
# files, so the whole run reads as one column of results. deploy-scripts/_ui.sh
# is deliberately not reused here: its run_step indents six spaces to sit under
# a `header`, and there is no header in this output.
RESULT_W=52
step_ok()   { printf '%s %-*s %s\n' "${C_GRN}✔${C_RST}" "$RESULT_W" "$1" "$2"; }
step_fail() { printf '%s %-*s %s\n' "${C_RED}✘${C_RST}" "$RESULT_W" "$1" "$2"; }

# Run a setup command behind one progress line. Prints a result line with the
# duration, or the label plus the captured output when it fails.
step_run() { # $1 = label, rest = argv
	local label="$1"; shift
	local log start rc
	log=$(mktemp); start=$SECONDS
	if [ -t 1 ]; then printf '%s %-*s %s' "${C_DIM}…${C_RST}" "$RESULT_W" "$label" "${C_DIM}working${C_RST}"; fi
	"$@" >"$log" 2>&1; rc=$?
	if [ -t 1 ]; then printf '\r\033[2K'; fi
	if [ $rc -eq 0 ]; then
		step_ok "$label" "${C_DIM}$((SECONDS - start))s${C_RST}"
	else
		step_fail "$label" "${C_RED}exit ${rc}${C_RST}"
		cat "$log" >&2
	fi
	rm -f "$log"
	return $rc
}

if [ "$skip_build" -eq 0 ]; then
  step_run "building programs (sbpf v3)" bash deploy-scripts/build-sbf.sh test || exit 1
  step_run "generating IDL" bash -c 'bun run program:idl' || exit 1
else
  # --skip-build still needs the bundled SDK IDL to match the deployed program ID,
  # otherwise tx instructions target a program that LiteSVM never loaded. With the
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
  # Copy only when the content differs. An unconditional cp bumps the mtime and
  # makes the SDK build below look stale on every run.
  cmp -s target/idl/velocity.json packages/sdk/src/idl/velocity.json \
    || cp target/idl/velocity.json packages/sdk/src/idl/
  cmp -s target/types/velocity.ts packages/sdk/src/idl/velocity.ts \
    || cp target/types/velocity.ts packages/sdk/src/idl/
fi

# Build the SDK in both paths: many test files import the package root
# (`from '../packages/sdk'`), which resolves through package.json `main` to
# packages/sdk/lib/node/index.js. ts-mocha only transpiles the
# `../packages/sdk/src/...` imports on the fly, so without this build those
# bare-package imports fail with MODULE_NOT_FOUND in CI. Runs after the IDL is
# synced into src/idl/ above so lib/ reflects the freshly-built program.
#
# `bun run build` starts with `rm -rf lib`, so it is a full tsc every time, about
# seven seconds. Skip it when nothing under src/ is newer than the built
# entrypoint, and show a progress line when it does run: a silent multi-second
# pause before the first test looks like a hang.
sdk_lib_current() {
  local lib=packages/sdk/lib/node/index.js
  [ -f "$lib" ] || return 1
  [ -z "$(find packages/sdk/src packages/sdk/package.json packages/sdk/tsconfig.json \
            -newer "$lib" -print -quit 2>/dev/null)" ]
}

if sdk_lib_current; then
  step_ok "SDK build" "${C_DIM}up to date${C_RST}"
else
  step_run "SDK build" bash -c 'cd packages/sdk && bun run build' || exit 1
fi

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
  # postOnlyAmmFulfillment.ts
  # TODO BROKEN ^^
	builderCodes.ts
  decodeUser.ts
  initMarketV2.ts
  scaleOrders.ts
  admin.ts
  accountExtension.ts
  assetTier.ts
  cancelAllOrders.ts
  curve.ts
  deleteInitializedSpotMarket.ts
  depositIntoSpotMarketVault.ts
  equityFloor.ts
  equityFloorSwap.ts
  equityFloorLazyTrip.ts
  equityFloorOracle.ts
  equityFloorFillGates.ts
  equityBreakerFreeze.ts
  velocityClient.ts
  insuranceFundStake.ts
  isolatedPositionVelocityClient.ts
  isolatedPositionLiquidatePerp.ts
  isolatedPositionLiquidatePerpwithFill.ts
  bankruptcyIfFloor.ts
  liquidateBorrowForPerpPnl.ts
  liquidatePerp.ts
  liquidatePerpWithFill.ts
  liquidatePerpPnlForDeposit.ts
  liquidateSpot.ts
  liquidateSpotSocialLoss.ts
  liquidateSpotWithSwap.ts
  # lpPool.ts # depends on PerpMarket layout shift — needs re-snapshot
  # lpPoolSwap.ts # depends on PerpMarket layout shift — needs re-snapshot
  marketOrder.ts
  marketOrderBaseAssetAmount.ts
  maxDeposit.ts
  maxLeverageOrderParams.ts
  mmOracleBatchNative.ts
  modifyOrder.ts
  multipleMakerOrders.ts
  oracleDiffSources.ts
  oracleFillPriceGuardrails.ts
  oracleOffsetOrders.ts
  order.ts
  orderMarginChecks.ts
  isolatedTransferMarginChecks.ts
  ordersWithSpread.ts
  pauseExchange.ts
  pauseDepositWithdraw.ts
  placeAndMakePerp.ts
  placeAndMakeSignedMsgSvm.ts
  jitProxy.ts
  postOnly.ts
  prelisting.ts
  pyth.ts
  pythLazerSvm.ts
  referrer.ts
  roundInFavorBaseAsset.ts
  settlePNLInvariant.ts
  spotDepositWithdraw.ts
  spotDepositWithdraw22.ts
  spotDepositWithdraw22TransferHooks.ts
  spotMarketPoolIds.ts
  # spotSwap.ts # broken by spot fulfillment purge — needs migration to read serum vaults directly off the Market
  # spotSwap22.ts # broken by spot fulfillment purge — needs migration to read serum vaults directly off the Market
  swapPostEndIxs.ts
  stopLimits.ts
  subaccounts.ts
  surgePricing.ts
  switchOracle.ts
  triggerOrders.ts
  transferPerpPosition.ts
  userAccount.ts
  userDelegate.ts
  userOrderId.ts
  # perpMarketConfig.ts # market_config field reads as 0 after write — possibly fetch caching or layout mismatch with reordered PerpMarket

  # whitelist.ts
  transferFeeAndPnlPool.ts
  protocolFees.ts
  recenterAmmCrankOracle.ts
  specialUserAccount.ts
)

# SINGLE_PROCESS=1 runs every file in one mocha process instead of one process
# per file. Each file still gets its own LiteSVM via startLiteSVM(), which costs
# ~40ms, so the isolation that matters is preserved; what you stop paying is the
# ~1.6s of importing packages/sdk/src, once per process. Measured on this suite:
# 53s to 18.5s, with identical results over four runs.
#
# It is not the default yet. One process means no isolation of module-level
# state between files, and the failure that would introduce is order-dependent
# flakiness, which is miserable to diagnose. Use it as the local loop, leave CI
# on the per-file gate, and switch once it has been boring for a few weeks.
if [ "${SINGLE_PROCESS:-}" = "1" ]; then
  printf '%s\n' "${C_DIM}  single process, ${#test_files[@]} files${C_RST}"
  prefixed=()
  for f in "${test_files[@]}"; do prefixed+=("./tests/velocity/$f"); done
  sp_log=$(mktemp)
  # Same contract as the per-file path below. Tests and the SDK log heavily to
  # stdout and stderr, which buries the result, so all of it goes to a file that
  # is printed only on failure.
  set +e
  # The reporter marks its own lines with \x01. Everything goes to the log; only
  # the marked lines reach the terminal, with the marker stripped.
  ts-mocha --exit -t 300000 \
    --reporter test-scripts/mocha-file-reporter.cjs \
    "${prefixed[@]}" 2>&1 \
    | tee "$sp_log" \
    | grep --line-buffered -a $'^\x01' \
    | sed $'s/\x01//'
  sp_status=${PIPESTATUS[0]}
  set -e
  if [ "$sp_status" -ne 0 ]; then
    echo ""
    echo "══════════════════════════════════════"
    echo "  FAILED, captured stdout below"
    echo "══════════════════════════════════════"
    sed $'s/\x01//' "$sp_log"
  fi
  rm -f "$sp_log"
  exit "$sp_status"
fi

# Run up to PARALLEL tests concurrently. Output is buffered per test and only
# printed on failure so interleaved stdout from concurrent processes doesn't
# obscure which test failed.
PARALLEL=${PARALLEL:-4}
tmpdir=$(mktemp -d)
trap "rm -rf '$tmpdir'" EXIT

declare -a q_pids=()
declare -a q_files=()
declare -a q_logs=()
overall_failed=0
files_passed=0
tests_passed=0
tests_failed=0
tests_pending=0
suite_start=$SECONDS

# Each child emits one machine-readable totals line; add it to the running sum.
tally() {
  local line
  line=$(grep -a $'^\x01\x02' "$1" | tail -1 | tr -d $'\x01\x02') || return 0
  [ -n "$line" ] || return 0
  tests_passed=$((tests_passed + $(echo "$line" | cut -d' ' -f1)))
  tests_failed=$((tests_failed + $(echo "$line" | cut -d' ' -f2)))
  tests_pending=$((tests_pending + $(echo "$line" | cut -d' ' -f3)))
}


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
  # The child ran the same reporter as the single-process path, so it already
  # rendered its own result line; pull it out of the log and pass it through.
  if wait "$pid"; then
    files_passed=$((files_passed + 1))
    tally "$log"
    grep -a $'^\x01' "$log" | grep -av $'^\x01\x02' | sed $'s/\x01//' || true
  else
    tally "$log"
    grep -a $'^\x01' "$log" | grep -av $'^\x01\x02' | sed $'s/\x01//' || true
    echo ""
    echo "${C_RED}══════════════════════════════════════${C_RST}"
    echo "  ${C_RED}FAILED${C_RST} ${C_BOLD}${file}${C_RST}, captured output below"
    echo "${C_RED}══════════════════════════════════════${C_RST}"
    grep -av $'^\x01' "$log" || true
    overall_failed=1
  fi
}

for test_file in "${test_files[@]}"; do
  [ $overall_failed -eq 1 ] && break
  while [ ${#q_pids[@]} -ge $PARALLEL ]; do
    collect_any
    [ $overall_failed -eq 1 ] && break 2
  done
  log="$tmpdir/${test_file}"
  TEST_REPORT_TOTALS=0 ts-mocha --exit -t 300000 \
    --reporter test-scripts/mocha-file-reporter.cjs \
    "./tests/velocity/$test_file" >"$log" 2>&1 &
  q_pids+=($!)
  q_files+=("$test_file")
  q_logs+=("$log")
done

while [ ${#q_pids[@]} -gt 0 ]; do
  collect_any
done

suite_secs=$((SECONDS - suite_start))
summary="  ${C_GRN}${tests_passed} passed${C_RST}"
[ "$tests_failed" -gt 0 ] && summary="$summary  ${C_RED}${tests_failed} failed${C_RST}"
[ "$tests_pending" -gt 0 ] && summary="$summary  ${C_DIM}${tests_pending} pending${C_RST}"
summary="$summary  ${C_DIM}across ${files_passed} files in ${suite_secs}s${C_RST}"
echo ""
echo "$summary"

[ $overall_failed -eq 0 ] || exit 1
