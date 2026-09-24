import { expect } from 'chai';

import {
	selectLiquidationExitCandidates,
	LiquidationExitPosition,
} from './liquidator';

const ALL_MARKETS = [0, 1, 2, 3];

function position(
	marketIndex: number,
	overrides: Partial<LiquidationExitPosition> = {}
): LiquidationExitPosition {
	return {
		marketIndex,
		hasBase: false,
		hasOpenOrder: false,
		hasPnl: false,
		isolated: false,
		flagged: false,
		...overrides,
	};
}

describe('selectLiquidationExitCandidates', () => {
	it('cranks a market holding orders but no base', () => {
		const candidates = selectLiquidationExitCandidates(
			[position(2, { hasOpenOrder: true })],
			true,
			ALL_MARKETS
		);

		expect(candidates[0]).to.deep.equal({ kind: 'crank', marketIndex: 2 });
	});

	it('sizes a real liquidation for a base position', () => {
		const candidates = selectLiquidationExitCandidates(
			[position(1, { hasBase: true, hasOpenOrder: true })],
			true,
			ALL_MARKETS
		);

		// Orders next to a base position still need a sized amount. The program
		// rejects a zero base amount once it reaches the transfer.
		expect(candidates[0]).to.deep.equal({ kind: 'base', marketIndex: 1 });
	});

	it('routes a pnl-only position away from liquidate_perp', () => {
		const candidates = selectLiquidationExitCandidates(
			[position(0, { hasPnl: true })],
			true,
			ALL_MARKETS
		);

		expect(candidates[0]).to.deep.equal({ kind: 'pnl', marketIndex: 0 });
	});

	it('falls back to a market with no position when nothing is actionable', () => {
		// The shape a user is left in once their last position is settled away.
		const candidates = selectLiquidationExitCandidates([], true, ALL_MARKETS);

		expect(candidates).to.deep.equal([{ kind: 'crank', marketIndex: 0 }]);
	});

	it('always ends on the empty-market crank so the flag can still clear', () => {
		const candidates = selectLiquidationExitCandidates(
			[position(0, { hasPnl: true })],
			true,
			ALL_MARKETS
		);

		expect(candidates).to.deep.equal([
			{ kind: 'pnl', marketIndex: 0 },
			{ kind: 'crank', marketIndex: 1 },
		]);
	});

	it('orders candidates cheapest first', () => {
		const candidates = selectLiquidationExitCandidates(
			[
				position(0, { hasPnl: true }),
				position(1, { hasBase: true }),
				position(2, { hasOpenOrder: true }),
			],
			true,
			ALL_MARKETS
		);

		expect(candidates.map((c) => c.kind)).to.deep.equal([
			'crank',
			'base',
			'pnl',
			'crank',
		]);
	});

	it('lists every base position so an unusable first one is not a dead end', () => {
		const candidates = selectLiquidationExitCandidates(
			[position(1, { hasBase: true }), position(2, { hasBase: true })],
			true,
			ALL_MARKETS
		);

		expect(candidates.slice(0, 2)).to.deep.equal([
			{ kind: 'base', marketIndex: 1 },
			{ kind: 'base', marketIndex: 2 },
		]);
	});

	it('drops markets this bot cannot liquidate', () => {
		const candidates = selectLiquidationExitCandidates(
			[position(3, { hasBase: true })],
			true,
			[0, 1]
		);

		// Market 3 is paused or unconfigured, so the empty-market crank is the
		// only candidate left. Any other call would revert every tick.
		expect(candidates).to.deep.equal([{ kind: 'crank', marketIndex: 0 }]);
	});

	it('returns nothing when no market is actionable', () => {
		expect(
			selectLiquidationExitCandidates(
				[position(3, { hasBase: true })],
				true,
				[]
			)
		).to.deep.equal([]);
	});

	describe('isolated positions', () => {
		it('targets the flagged isolated market itself', () => {
			// The program reads the per-position flag when the target market holds
			// an isolated position, so only market 2 can clear market 2.
			const candidates = selectLiquidationExitCandidates(
				[position(2, { hasBase: true, isolated: true, flagged: true })],
				false,
				ALL_MARKETS
			);

			expect(candidates).to.deep.equal([{ kind: 'base', marketIndex: 2 }]);
		});

		it('never offers an isolated market for the account-level flag', () => {
			// Targeting the isolated position switches the program to isolated
			// mode. That mode reads a flag which is not set, so the call fails
			// with SufficientCollateral and clears nothing.
			const candidates = selectLiquidationExitCandidates(
				[position(1, { hasBase: true, isolated: true })],
				true,
				ALL_MARKETS
			);

			expect(candidates).to.deep.equal([{ kind: 'crank', marketIndex: 0 }]);
		});

		it('handles both flags at once, isolated scope first', () => {
			const candidates = selectLiquidationExitCandidates(
				[
					position(0, { hasBase: true }),
					position(1, { hasBase: true, isolated: true, flagged: true }),
				],
				true,
				ALL_MARKETS
			);

			expect(candidates).to.deep.equal([
				{ kind: 'base', marketIndex: 1 },
				{ kind: 'base', marketIndex: 0 },
				{ kind: 'crank', marketIndex: 2 },
			]);
		});

		it('offers nothing when an unflagged isolated position is all there is', () => {
			expect(
				selectLiquidationExitCandidates(
					[position(0, { hasBase: true, isolated: true })],
					false,
					ALL_MARKETS
				)
			).to.deep.equal([]);
		});
	});
});
