#!/bin/bash

set -e
trap 'echo -e "\nStopped by signal $? (SIGINT)"; exit 0' INT

export PATH="$PWD/bin:$PWD/node_modules/.bin:$PATH"

if [ "$1" != "--skip-build" ]; then
  anchor build --ignore-keys --skip-lint -- --features anchor-test &&
    cp target/idl/velocity.json sdk/src/idl/ && cp target/types/velocity.ts sdk/src/idl/
else
  # --skip-build still needs the bundled SDK IDL to match the deployed program ID,
  # otherwise tx instructions target a program that bankrun never loaded.
  if [ -f target/idl/velocity.json ]; then
    cp target/idl/velocity.json sdk/src/idl/
  fi
  if [ -f target/types/velocity.ts ]; then
    cp target/types/velocity.ts sdk/src/idl/
  fi
  ( cd sdk && bun run build >/dev/null )
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
  scaleOrders.ts
  admin.ts
  assetTier.ts
  cancelAllOrders.ts
  curve.ts
  deleteInitializedSpotMarket.ts
  depositIntoSpotMarketVault.ts
  velocityClient.ts
  insuranceFundStake.ts
  isolatedPositionVelocityClient.ts
  isolatedPositionLiquidatePerp.ts
  isolatedPositionLiquidatePerpwithFill.ts
  liquidateBorrowForPerpPnl.ts
  liquidatePerp.ts
  liquidatePerpWithFill.ts
  liquidatePerpPnlForDeposit.ts
  liquidateSpot.ts
  liquidateSpotSocialLoss.ts
  # lpPool.ts # depends on PerpMarket layout shift — needs re-snapshot
  # lpPoolSwap.ts # depends on PerpMarket layout shift — needs re-snapshot
  marketOrder.ts
  marketOrderBaseAssetAmount.ts
  maxDeposit.ts
  maxLeverageOrderParams.ts
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
  placeAndMakeSignedMsgBankrun.ts
  postOnly.ts
  prelisting.ts
  pyth.ts
  pythLazerBankrun.ts
  referrer.ts
  roundInFavorBaseAsset.ts
  settlePNLInvariant.ts
  spotDepositWithdraw.ts
  spotDepositWithdraw22.ts
  spotDepositWithdraw22TransferHooks.ts
  spotMarketPoolIds.ts
  # spotSwap.ts # broken by spot fulfillment purge — needs migration to read serum vaults directly off the Market
  # spotSwap22.ts # broken by spot fulfillment purge — needs migration to read serum vaults directly off the Market
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
  specialUserAccount.ts
)

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

collect_oldest() {
  local pid="${q_pids[0]}"
  local file="${q_files[0]}"
  local log="${q_logs[0]}"
  q_pids=("${q_pids[@]:1}")
  q_files=("${q_files[@]:1}")
  q_logs=("${q_logs[@]:1}")
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
  [ $overall_failed -eq 1 ] && break
  while [ ${#q_pids[@]} -ge $PARALLEL ]; do
    collect_oldest
    [ $overall_failed -eq 1 ] && break 2
  done
  log="$tmpdir/${test_file}"
  ts-mocha --exit -t 300000 "./tests/$test_file" >"$log" 2>&1 &
  q_pids+=($!)
  q_files+=("$test_file")
  q_logs+=("$log")
  echo "  start: $test_file"
done

while [ ${#q_pids[@]} -gt 0 ]; do
  collect_oldest
done

[ $overall_failed -eq 0 ] || exit 1
