---
'@velocity-exchange/sdk': patch
'@velocity-exchange/admin-cli': patch
---

Program↔SDK parity fixes from the 2026-07-02 audit: renamed deprecated Switchboard
OracleSource keys to match the IDL (fixes a decode crash on affected markets), applied
the $100 initial-margin unrealized-PnL cap, standardized auction/limit prices to order
tick size across the DLOB, isolated-position handling in bankruptcy/liquidation math,
corrected MM-oracle validity gating, referrer_status memcmp offset, PerpOperation and
OrderBitFlag bit values, wired five missing event records into EventSubscriber, fixed
withdraw-limit divisors, multi-pool margin segregation, referee/builder fee estimation,
and added AdminClient.updatePauseAdmin plus admin CLI commands for pause-admin rotation
and fee-pool transfers. Also fixed withdrawFromIsolatedPerpPosition's withdraw-all path:
it substituted the MIN_I64 sentinel into the instruction's unsigned u64 amount (serializing
as 2^63, so full withdrawals always failed on-chain with InsufficientCollateral); it now
clamps the request to the position's deposit plus claimable PnL.

Follow-up completeness fixes: DLOBSubscriber.getL2/getL3 (and the dlob-server publisher) now
thread orderTickSize so the public book view is tick-standardized like on-chain; added
hasIsolatedMarginBankrupt and wired isolated-only bankruptcy detection into keeper resolution
(isIsolatedPositionBankrupt now guards against non-isolated indices); getMarketFees applies the
referee discount and calculateFeeForQuoteAmount accepts builder params so both public fee-prediction
entry points match on-chain; and isFallbackAvailableLiquiditySource now fully mirrors
amm_fill_gates_ok, adding the market-drawdown and MM-vs-exchange oracle volatility gates.
