/**
 * Maker-facing abstraction over the per-subaccount equity floor. The on-chain
 * checks are deliberately per-subaccount (each check reads only the one User
 * account already loaded in the hot path), which leaves the burden of placing
 * floor where the equity is on the delegate. This module removes that burden:
 * it treats an authority's subaccounts as one pool, plans quote transfers with
 * the exact floor delta they must carry, computes how much can leave a
 * subaccount, and rebalances the floor split to match where the equity
 * actually sits — so a delegate never has to reason about floor placement to
 * stay clear of the breaker.
 */
import { PublicKey } from '@solana/web3.js';
import { TransactionSignature } from '@solana/web3.js';
import { BN } from './isomorphic/anchor';
import { QUOTE_PRECISION, ZERO } from './constants/numericConstants';
import { QUOTE_SPOT_MARKET_INDEX } from './constants/numericConstants';
import {
	calculateEquityFloorAutoDelta,
	getEquityFloorLevel,
	EquityFloorLevel,
} from './math/margin';
import { User } from './user';
import { VelocityClient } from './velocityClient';
import { TxParams } from './types';

/** One subaccount's standing relative to its floor. All BN values QUOTE_PRECISION. */
export type SubaccountFloorStatus = {
	subAccountId: number;
	/** Net equity from `User.getFloorNetEquity()`, the unweighted value the onchain floor checks read. */
	equity: BN;
	equityFloor: BN;
	equityFloorBuffer: BN;
	/** `equityFloor + equityFloorBuffer`: what risk-increasing actions must clear. */
	bufferedFloor: BN;
	/** `equity - equityFloor`; negative means the breaker can trip on this subaccount. */
	headroom: BN;
	/** `equity - bufferedFloor`; negative means risk-increasing actions are rejecting. */
	bufferedHeadroom: BN;
	level: EquityFloorLevel;
};

/** Authority-wide standing: aggregates plus the per-subaccount breakdown. */
export type EquityFloorStatus = {
	authority: PublicKey;
	breakerTripped: boolean;
	totalEquity: BN;
	totalFloor: BN;
	totalBuffer: BN;
	/** `totalEquity - (totalFloor + totalBuffer)`: the slack the whole pool has to allocate. */
	totalBufferedHeadroom: BN;
	/** Worst level across subaccounts with a floor set. */
	level: EquityFloorLevel;
	subaccounts: SubaccountFloorStatus[];
};

/** A floor-only rebalance step: zero-amount `transferDepositByDelegate` carrying `equityFloorDelta`. */
export type FloorMove = {
	fromSubAccountId: number;
	toSubAccountId: number;
	equityFloorDelta: BN;
};

/** A fully resolved quote transfer ready to submit. */
export type QuoteTransferPlan = {
	amount: BN;
	marketIndex: number;
	fromSubAccountId: number;
	toSubAccountId: number;
	equityFloorDelta: BN;
};

/**
 * Splits `totalFloor` across subaccounts proportionally to their equity,
 * clamped so every allocation is backed (`floor_i <= max(0, equity_i -
 * buffer_i)`), with the clamped remainder water-filled into subaccounts that
 * still have capacity. Entries in `pinned` keep their current floor and
 * receive none of the remainder (used for breached subaccounts, which cannot
 * shed floor on-chain). Returns `null` when no backed allocation exists, i.e.
 * the pool's equity cannot cover `totalFloor` plus buffers.
 */
export function allocateEquityFloors(
	totalFloor: BN,
	subaccounts: { equity: BN; buffer: BN; currentFloor: BN }[],
	pinned: boolean[] = []
): BN[] | null {
	const n = subaccounts.length;
	const targets: BN[] = new Array(n).fill(ZERO);

	let remaining = totalFloor;
	for (let i = 0; i < n; i++) {
		if (pinned[i]) {
			targets[i] = subaccounts[i].currentFloor;
			remaining = remaining.sub(subaccounts[i].currentFloor);
		}
	}
	if (remaining.isNeg()) {
		// pinned floors alone exceed the total: nothing to allocate elsewhere
		return null;
	}

	const free = [...Array(n).keys()].filter((i) => !pinned[i]);
	const caps = subaccounts.map((s, i) =>
		pinned[i] ? ZERO : BN.max(s.equity.sub(s.buffer), ZERO)
	);
	const totalCap = free.reduce((sum, i) => sum.add(caps[i]), ZERO);
	if (totalCap.lt(remaining)) {
		return null;
	}

	const totalFreeEquity = free.reduce(
		(sum, i) => sum.add(BN.max(subaccounts[i].equity, ZERO)),
		ZERO
	);
	// proportional-to-equity first pass (floor division), clamped to capacity
	let assigned = ZERO;
	for (const i of free) {
		const share = totalFreeEquity.gt(ZERO)
			? remaining.mul(BN.max(subaccounts[i].equity, ZERO)).div(totalFreeEquity)
			: ZERO;
		targets[i] = BN.min(share, caps[i]);
		assigned = assigned.add(targets[i]);
	}
	// water-fill the rounding/clamping remainder into leftover capacity
	let leftover = remaining.sub(assigned);
	for (const i of free) {
		if (leftover.lte(ZERO)) {
			break;
		}
		const slack = caps[i].sub(targets[i]);
		const add = BN.min(slack, leftover);
		targets[i] = targets[i].add(add);
		leftover = leftover.sub(add);
	}
	if (leftover.gt(ZERO)) {
		return null;
	}
	return targets;
}

/**
 * Turns a current → target floor split into concrete moves, greedily matching
 * surpluses against deficits. The moves conserve the floor sum by
 * construction and each one only ever sheds floor from a subaccount whose
 * floor is above its target.
 */
export function planFloorMoves(
	subAccountIds: number[],
	current: BN[],
	target: BN[]
): FloorMove[] {
	const surpluses: { index: number; amount: BN }[] = [];
	const deficits: { index: number; amount: BN }[] = [];
	for (let i = 0; i < current.length; i++) {
		const diff = current[i].sub(target[i]);
		if (diff.gt(ZERO)) {
			surpluses.push({ index: i, amount: diff });
		} else if (diff.lt(ZERO)) {
			deficits.push({ index: i, amount: diff.neg() });
		}
	}

	const moves: FloorMove[] = [];
	let s = 0;
	let d = 0;
	while (s < surpluses.length && d < deficits.length) {
		const delta = BN.min(surpluses[s].amount, deficits[d].amount);
		moves.push({
			fromSubAccountId: subAccountIds[surpluses[s].index],
			toSubAccountId: subAccountIds[deficits[d].index],
			equityFloorDelta: delta,
		});
		surpluses[s].amount = surpluses[s].amount.sub(delta);
		deficits[d].amount = deficits[d].amount.sub(delta);
		if (surpluses[s].amount.isZero()) {
			s++;
		}
		if (deficits[d].amount.isZero()) {
			d++;
		}
	}
	return moves;
}

/**
 * Plans the fund-only transfers (zero floor delta) that lift subaccounts below
 * their buffered floor back above it. The equity comes from the spare equity of
 * the other subaccounts. Each deficit side lands `haircut` above its buffered
 * floor. Each donor side keeps `haircut` above its own, so a cure cannot open a
 * new breach. When spare equity cannot cover every deficit, the deepest
 * breaches are filled first and the rest needs a fresh deposit.
 */
export function planCureMoves(
	subaccounts: Pick<
		SubaccountFloorStatus,
		'subAccountId' | 'equityFloor' | 'bufferedHeadroom'
	>[],
	haircut: BN
): QuoteTransferPlan[] {
	// Worst first, so scarce donor equity reaches the deepest breach.
	const deficits = subaccounts
		.filter((u) => u.equityFloor.gt(ZERO) && u.bufferedHeadroom.isNeg())
		.sort((a, b) => a.bufferedHeadroom.cmp(b.bufferedHeadroom))
		.map((u) => ({
			subAccountId: u.subAccountId,
			// Land above the gate by the same haircut the transfer applies.
			amount: u.bufferedHeadroom.neg().add(haircut),
		}));
	const donors = subaccounts
		.map((u) => ({
			subAccountId: u.subAccountId,
			amount: BN.max(u.bufferedHeadroom.sub(haircut), ZERO),
		}))
		.filter((d) => d.amount.gt(ZERO))
		.sort((a, b) => b.amount.cmp(a.amount));

	const plans: QuoteTransferPlan[] = [];
	let s = 0;
	let d = 0;
	while (s < donors.length && d < deficits.length) {
		const amount = BN.min(donors[s].amount, deficits[d].amount);
		plans.push({
			amount,
			marketIndex: QUOTE_SPOT_MARKET_INDEX,
			fromSubAccountId: donors[s].subAccountId,
			toSubAccountId: deficits[d].subAccountId,
			equityFloorDelta: ZERO,
		});
		donors[s].amount = donors[s].amount.sub(amount);
		deficits[d].amount = deficits[d].amount.sub(amount);
		if (donors[s].amount.isZero()) {
			s++;
		}
		if (deficits[d].amount.isZero()) {
			d++;
		}
	}
	return plans;
}

const LEVEL_SEVERITY: Record<EquityFloorLevel, number> = {
	breached: 4,
	critical: 3,
	warning: 2,
	healthy: 1,
	disabled: 0,
};

export type EquityFloorManagerConfig = {
	/**
	 * Subaccount ids to manage. Defaults to every subaccount of the client's
	 * authority currently subscribed on the `VelocityClient`.
	 */
	subAccountIds?: number[];
	/**
	 * Client-side equity haircut (QUOTE_PRECISION) absorbing dust from onchain/client oracle
	 * price differences when sizing floor deltas. Defaults to 1 quote unit ($1).
	 */
	collateralHaircut?: BN;
};

/**
 * See the module doc. All reads use the subaccounts already subscribed on the
 * wrapped `VelocityClient` (the delegate's client, whose `authority` is the
 * subaccounts' owner); all writes go through `transferDepositByDelegate`.
 */
export class EquityFloorManager {
	private collateralHaircut: BN;
	private subAccountIds?: number[];

	public constructor(
		private velocityClient: VelocityClient,
		config: EquityFloorManagerConfig = {}
	) {
		this.subAccountIds = config.subAccountIds;
		this.collateralHaircut = config.collateralHaircut ?? QUOTE_PRECISION;
	}

	private getManagedUsers(): User[] {
		const authority = this.velocityClient.authority;
		let users = this.velocityClient
			.getUsers()
			.filter((user) =>
				user.getUserAccountOrThrow().authority.equals(authority)
			);
		if (this.subAccountIds !== undefined) {
			const wanted = new Set(this.subAccountIds);
			users = users.filter((user) =>
				wanted.has(user.getUserAccountOrThrow().subAccountId)
			);
		}
		return users.sort(
			(a, b) =>
				a.getUserAccountOrThrow().subAccountId -
				b.getUserAccountOrThrow().subAccountId
		);
	}

	private getSubaccountStatus(user: User): SubaccountFloorStatus {
		const userAccount = user.getUserAccountOrThrow();
		// Price the account the way the onchain floor gate does. With no slot
		// every oracle counts as valid.
		const equity = user.getFloorNetEquity().value;
		const bufferedFloor = userAccount.equityFloor.add(
			userAccount.equityFloorBuffer
		);
		return {
			subAccountId: userAccount.subAccountId,
			equity,
			equityFloor: userAccount.equityFloor,
			equityFloorBuffer: userAccount.equityFloorBuffer,
			bufferedFloor,
			headroom: equity.sub(userAccount.equityFloor),
			bufferedHeadroom: equity.sub(bufferedFloor),
			level: getEquityFloorLevel(
				equity,
				userAccount.equityFloor,
				userAccount.equityFloorBuffer
			),
		};
	}

	/** Full authority-wide standing: aggregates, worst level, per-subaccount detail. */
	public getStatus(): EquityFloorStatus {
		const subaccounts = this.getManagedUsers().map((user) =>
			this.getSubaccountStatus(user)
		);
		const totalEquity = subaccounts.reduce((s, u) => s.add(u.equity), ZERO);
		const totalFloor = subaccounts.reduce((s, u) => s.add(u.equityFloor), ZERO);
		const totalBuffer = subaccounts.reduce(
			(s, u) => s.add(u.equityFloorBuffer),
			ZERO
		);
		const level = subaccounts.reduce<EquityFloorLevel>(
			(worst, u) =>
				LEVEL_SEVERITY[u.level] > LEVEL_SEVERITY[worst] ? u.level : worst,
			'disabled'
		);
		return {
			authority: this.velocityClient.authority,
			breakerTripped:
				(this.velocityClient.getUserStats()?.getAccount()
					?.equityBreakerTripped ?? 0) !== 0,
			totalEquity,
			totalFloor,
			totalBuffer,
			totalBufferedHeadroom: totalEquity.sub(totalFloor).sub(totalBuffer),
			level,
			subaccounts,
		};
	}

	/**
	 * The most quote that can leave `subAccountId` to the outside (a
	 * withdrawal, which cannot move floor): equity above the buffered floor,
	 * less the haircut. Floor constraint only — the withdrawal itself is still
	 * subject to margin and borrow limits.
	 */
	public getMaxWithdrawable(subAccountId: number): BN {
		const status = this.getSubaccountStatus(
			this.velocityClient.getUser(subAccountId, this.velocityClient.authority)
		);
		if (status.equityFloor.lte(ZERO)) {
			return status.equity;
		}
		return BN.max(status.bufferedHeadroom.sub(this.collateralHaircut), ZERO);
	}

	/**
	 * The most quote that can move from one subaccount to another when the
	 * transfer carries floor with it. Because floor travels with the funds
	 * (capped at the floor the debited side holds, after which its check
	 * disables entirely), this is normally the debited side's whole equity —
	 * bounded by what the credited side's equity can back. Floor constraint
	 * only; margin and borrow limits still apply on top.
	 */
	public getMaxQuoteTransferable(
		fromSubAccountId: number,
		toSubAccountId: number
	): BN {
		const from = this.getSubaccountStatus(
			this.velocityClient.getUser(
				fromSubAccountId,
				this.velocityClient.authority
			)
		);
		const to = this.getSubaccountStatus(
			this.velocityClient.getUser(toSubAccountId, this.velocityClient.authority)
		);
		const haircut = this.collateralHaircut;

		// how much floor the credited side can absorb beyond what the incoming
		// funds themselves back: its own buffered headroom (delta <= amount
		// keeps it backed; beyond that it eats into existing headroom)
		const toSlack = BN.max(to.bufferedHeadroom.sub(haircut), ZERO);

		if (from.equityFloor.lte(ZERO)) {
			return BN.max(from.equity.sub(haircut), ZERO);
		}

		// shedding the entire floor disables the debited side's check; possible
		// only if the credited side can absorb floor faster than the funds back
		// it, i.e. it has slack of its own
		const fullShedViable = from.equity
			.sub(from.bufferedFloor)
			.add(from.equityFloor)
			.add(toSlack);
		// without full shed: amount <= excess + floor (auto delta caps at floor)
		const partialShedMax = BN.max(from.bufferedHeadroom, ZERO).add(
			from.equityFloor
		);
		const floorwiseMax = BN.min(
			BN.max(fullShedViable, partialShedMax),
			from.equity
		);
		return BN.max(floorwiseMax.sub(haircut), ZERO);
	}

	/**
	 * Resolves a quote transfer into the exact instruction parameters,
	 * padding the auto floor delta by the haircut so onchain pricing dust
	 * cannot fail it. The padded delta never exceeds the amount or the
	 * debited side's floor, so the credited side stays backed whenever it was
	 * before.
	 */
	public planQuoteTransfer(
		amount: BN,
		fromSubAccountId: number,
		toSubAccountId: number
	): QuoteTransferPlan {
		const fromUser = this.velocityClient.getUser(
			fromSubAccountId,
			this.velocityClient.authority
		);
		const fromAccount = fromUser.getUserAccountOrThrow();
		const equityFloorDelta = calculateEquityFloorAutoDelta(
			amount,
			// The haircut still pads for price movement between planning and
			// execution.
			fromUser.getFloorNetEquity().value.sub(this.collateralHaircut),
			fromAccount.equityFloor,
			fromAccount.equityFloorBuffer
		);
		return {
			amount,
			marketIndex: QUOTE_SPOT_MARKET_INDEX,
			fromSubAccountId,
			toSubAccountId,
			equityFloorDelta: BN.min(equityFloorDelta, amount),
		};
	}

	/** Plans and submits a quote transfer between two subaccounts in one call. */
	public async transferQuote(
		amount: BN,
		fromSubAccountId: number,
		toSubAccountId: number,
		txParams?: TxParams
	): Promise<TransactionSignature> {
		const plan = this.planQuoteTransfer(
			amount,
			fromSubAccountId,
			toSubAccountId
		);
		return this.velocityClient.transferDepositByDelegate(
			plan.amount,
			plan.marketIndex,
			plan.fromSubAccountId,
			plan.toSubAccountId,
			plan.equityFloorDelta,
			txParams
		);
	}

	/**
	 * Plans the floor-only moves (zero-amount transfers) that re-split the
	 * total floor proportionally to where the equity currently sits, so every
	 * subaccount ends with the same relative headroom. Breached subaccounts
	 * (below their raw floor) cannot shed floor on-chain, so their floor is
	 * pinned in place and the rest is allocated around them. Throws when the
	 * pool's equity cannot back the total floor plus buffers — at that point
	 * no split works and equity must be deposited (or the admin must lower
	 * the floor). Each onchain move also carries a proportional share of the
	 * debited side's buffer, so buffers drift toward the same split as the
	 * floors. The plan sizes against current buffers and the haircut absorbs
	 * the drift dust. A move can still revert when a credited side cannot back
	 * the buffer share it receives.
	 */
	public planFloorRebalance(): FloorMove[] {
		const subaccounts = this.getManagedUsers().map((user) =>
			this.getSubaccountStatus(user)
		);
		if (subaccounts.length < 2) {
			return [];
		}
		const totalFloor = subaccounts.reduce(
			(sum, u) => sum.add(u.equityFloor),
			ZERO
		);
		if (totalFloor.lte(ZERO)) {
			return [];
		}
		const pinned = subaccounts.map((u) => u.level === 'breached');
		const targets = allocateEquityFloors(
			totalFloor,
			subaccounts.map((u) => ({
				equity: BN.max(u.equity.sub(this.collateralHaircut), ZERO),
				buffer: u.equityFloorBuffer,
				currentFloor: u.equityFloor,
			})),
			pinned
		);
		if (targets === null) {
			throw new Error(
				'no backed floor split exists: total equity cannot cover the total floor plus buffers; deposit equity or have the admin lower the floor'
			);
		}
		return planFloorMoves(
			subaccounts.map((u) => u.subAccountId),
			subaccounts.map((u) => u.equityFloor),
			targets
		);
	}

	/**
	 * Plans fund-only transfers, the one delegate transfer allowed while the equity breaker
	 * is tripped. Does not clear the flag itself; see `planCureMoves` for sizing rules.
	 */
	public planCureTransfers(): QuoteTransferPlan[] {
		return planCureMoves(
			this.getManagedUsers().map((user) => this.getSubaccountStatus(user)),
			this.collateralHaircut
		);
	}

	/**
	 * Executes `planCureTransfers` serially. It works while the breaker is
	 * tripped. It does nothing when no subaccount is below its buffered floor.
	 */
	public async cureBreaches(
		txParams?: TxParams
	): Promise<TransactionSignature[]> {
		const sigs: TransactionSignature[] = [];
		for (const plan of this.planCureTransfers()) {
			sigs.push(
				await this.velocityClient.transferDepositByDelegate(
					plan.amount,
					plan.marketIndex,
					plan.fromSubAccountId,
					plan.toSubAccountId,
					plan.equityFloorDelta,
					txParams
				)
			);
		}
		return sigs;
	}

	/**
	 * Executes `planFloorRebalance` serially. Safe to run at any time; a
	 * no-op when the split already matches the equity distribution.
	 */
	public async rebalanceFloors(
		txParams?: TxParams
	): Promise<TransactionSignature[]> {
		const sigs: TransactionSignature[] = [];
		for (const move of this.planFloorRebalance()) {
			sigs.push(
				await this.velocityClient.transferDepositByDelegate(
					ZERO,
					QUOTE_SPOT_MARKET_INDEX,
					move.fromSubAccountId,
					move.toSubAccountId,
					move.equityFloorDelta,
					txParams
				)
			);
		}
		return sigs;
	}
}
