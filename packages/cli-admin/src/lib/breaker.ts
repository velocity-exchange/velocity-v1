import { BN } from '@coral-xyz/anchor';
import {
	SpotBalanceType,
	SpotMarketAccount,
	calculateWithdrawLimit,
	getTokenAmount,
} from '@velocity-exchange/sdk';

const TWENTY_FOUR_HOURS = 24 * 60 * 60;

/** The `withdraw_circuit_breaker_bps` the program applies when the field is 0. */
export const DEFAULT_WITHDRAW_BREAKER_BPS = 2500;

/**
 * The deposit token TWAP the next transaction on this market will see. The program only advances
 * `depositTokenTwap` when a transaction touches the market, so on a quiet market the stored value
 * lags deposits. This uses the weighting from `update_spot_market_twap_stats`, the same one
 * `calculateWithdrawLimit` and market-stats-bot use.
 */
export function liveDepositTokenTwap(
	market: SpotMarketAccount,
	now: number
): BN {
	const deposits = getTokenAmount(
		market.depositBalance,
		market,
		SpotBalanceType.DEPOSIT
	);
	const sinceLast = new BN(Math.max(0, now - market.lastTwapTs.toNumber()));
	const sinceStart = new BN(
		Math.max(0, TWENTY_FOUR_HOURS - sinceLast.toNumber())
	);
	return market.depositTokenTwap
		.mul(sinceStart)
		.add(deposits.mul(sinceLast))
		.div(sinceLast.add(sinceStart));
}

export interface BreakerState {
	/** Live deposit TWAP, in tokens. */
	twap: number;
	/** Current deposits, in tokens. */
	current: number;
	/** Deposits may not fall below this, in tokens. */
	minDeposit: number;
	/** Current borrows, in tokens. */
	borrows: number;
	/** Tokens users can still withdraw before the breaker blocks. Negative means already blocked. */
	headroom: number;
	/**
	 * Percent of the breaker consumed, matching the Grafana rule `Spot Withdraw Breaker
	 * {25,50,75}% Consumed`: clamp((twap - current) / clamp_min(twap - min_deposit, 1e-9), 0, 1).
	 */
	consumedPct: number;
}

/**
 * Withdraw circuit breaker state for a spot market at `now`. Pass `bps` to see what the market
 * would look like with a different `withdraw_circuit_breaker_bps`, where 0 means the 25% default.
 */
export function breakerState(
	market: SpotMarketAccount,
	now: number,
	bps: number = market.withdrawCircuitBreakerBps
): BreakerState {
	const div = 10 ** market.decimals;
	const { minDepositAmount, currentDepositAmount, currentBorrowAmount } =
		calculateWithdrawLimit(
			{ ...market, withdrawCircuitBreakerBps: bps },
			new BN(now)
		);
	const twap = Number(liveDepositTokenTwap(market, now).toString()) / div;
	const current = Number(currentDepositAmount.toString()) / div;
	const minDeposit = Number(minDepositAmount.toString()) / div;
	const denom = Math.max(twap - minDeposit, 1e-9);
	return {
		twap,
		current,
		minDeposit,
		borrows: Number(currentBorrowAmount.toString()) / div,
		headroom: current - minDeposit,
		consumedPct: Math.min(Math.max((twap - current) / denom, 0), 1) * 100,
	};
}
