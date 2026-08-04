import { BN } from '@coral-xyz/anchor';
import { assert } from 'chai';
import {
	calculateEquityFloorAutoDelta,
	getEquityFloorLevel,
	allocateEquityFloors,
	planFloorMoves,
	planCureMoves,
	ZERO,
} from '../../src';

const LCG_MUL = BigInt('6364136223846793005');
const LCG_ADD = BigInt('1442695040888963407');
const LCG_MASK = BigInt('0xffffffffffffffff');
const LCG_SHIFT = BigInt(33);

/** Deterministic LCG so the sequences are reproducible without a rand dep. */
class Lcg {
	private state: bigint;
	constructor(seed: number) {
		this.state = BigInt(seed);
	}
	next(modulus: number): number {
		this.state = (this.state * LCG_MUL + LCG_ADD) & LCG_MASK;
		return Number((this.state >> LCG_SHIFT) % BigInt(modulus));
	}
}

const bn = (n: number) => new BN(n);

describe('calculateEquityFloorAutoDelta', () => {
	it('returns zero when the floor is disabled', () => {
		assert(
			calculateEquityFloorAutoDelta(bn(1000), bn(50), ZERO, bn(500)).eq(ZERO)
		);
	});

	it('carries no floor while the transfer fits inside buffered headroom', () => {
		// equity 1000, floor 300, buffer 100 -> excess 600
		assert(
			calculateEquityFloorAutoDelta(bn(600), bn(1000), bn(300), bn(100)).eq(
				ZERO
			)
		);
	});

	it('carries the shortfall beyond buffered headroom, one for one', () => {
		// excess 600, amount 700 -> delta 100
		assert(
			calculateEquityFloorAutoDelta(bn(700), bn(1000), bn(300), bn(100)).eq(
				bn(100)
			)
		);
	});

	it('caps the delta at the floor the debited side holds', () => {
		// excess 600, amount 950 -> uncapped 350, capped at floor 300
		assert(
			calculateEquityFloorAutoDelta(bn(950), bn(1000), bn(300), bn(100)).eq(
				bn(300)
			)
		);
	});

	it('never exceeds amount, floor, or leaves a feasible debit side below its buffered floor', () => {
		const rng = new Lcg(0x5eedcafe);
		for (let i = 0; i < 20_000; i++) {
			const collateral = bn(rng.next(2_000_000));
			const floor = bn(rng.next(1_000_000));
			const buffer = bn(rng.next(200_000));
			const amount = bn(rng.next(2_000_000));

			const delta = calculateEquityFloorAutoDelta(
				amount,
				collateral,
				floor,
				buffer
			);

			assert(delta.gte(ZERO), `negative delta at iteration ${i}`);
			assert(delta.lte(amount), `delta above amount at iteration ${i}`);
			assert(delta.lte(floor), `delta above floor at iteration ${i}`);

			if (floor.isZero()) {
				assert(delta.isZero(), `delta with disabled floor at iteration ${i}`);
				continue;
			}

			const collateralAfter = collateral.sub(amount);
			const floorAfter = floor.sub(delta);
			// the debit side started at/above its buffered floor and the auto
			// delta did not hit the floor cap: it must end at/above its reduced
			// buffered floor (this is exactly what the on-chain check enforces)
			if (
				collateral.gte(floor.add(buffer)) &&
				delta.lt(floor) &&
				amount.lte(collateral)
			) {
				assert(
					collateralAfter.gte(floorAfter.add(buffer)),
					`auto delta leaves debit side below buffered floor at iteration ${i}: ` +
						`collateral ${collateral}, floor ${floor}, buffer ${buffer}, amount ${amount}, delta ${delta}`
				);
				// minimality: one less floor moved would breach, unless none was needed
				if (delta.gt(ZERO)) {
					assert(
						collateralAfter.lt(floorAfter.addn(1).add(buffer)),
						`auto delta is not minimal at iteration ${i}`
					);
				}
			}
		}
	});
});

describe('getEquityFloorLevel', () => {
	const floor = bn(1000);
	const buffer = bn(100);

	it('classifies every threshold boundary exactly', () => {
		assert.equal(getEquityFloorLevel(bn(999), floor, buffer), 'breached');
		assert.equal(getEquityFloorLevel(bn(1000), floor, buffer), 'critical');
		assert.equal(getEquityFloorLevel(bn(1099), floor, buffer), 'critical');
		assert.equal(getEquityFloorLevel(bn(1100), floor, buffer), 'warning');
		assert.equal(getEquityFloorLevel(bn(1199), floor, buffer), 'warning');
		assert.equal(getEquityFloorLevel(bn(1200), floor, buffer), 'healthy');
	});

	it('is disabled without a floor, regardless of buffer', () => {
		assert.equal(getEquityFloorLevel(bn(-5), ZERO, buffer), 'disabled');
	});

	it('honors a custom warning multiple', () => {
		assert.equal(getEquityFloorLevel(bn(1499), floor, buffer, 5), 'warning');
		assert.equal(getEquityFloorLevel(bn(1500), floor, buffer, 5), 'healthy');
	});

	it('collapses warning into critical when the buffer is zero', () => {
		assert.equal(getEquityFloorLevel(bn(999), floor, ZERO), 'breached');
		assert.equal(getEquityFloorLevel(bn(1000), floor, ZERO), 'healthy');
	});
});

describe('allocateEquityFloors', () => {
	it('splits proportionally to equity and preserves the total', () => {
		const targets = allocateEquityFloors(bn(700), [
			{ equity: bn(3000), buffer: bn(50), currentFloor: bn(700) },
			{ equity: bn(1000), buffer: bn(50), currentFloor: ZERO },
		]);
		assert(targets !== null);
		assert(targets![0].add(targets![1]).eq(bn(700)));
		// 3:1 equity split -> 525 / 175
		assert(targets![0].eq(bn(525)), `got ${targets![0]}`);
		assert(targets![1].eq(bn(175)), `got ${targets![1]}`);
	});

	it('clamps to capacity and water-fills the remainder', () => {
		// proportional would give sub 0 600, but its capacity is 3000-2900=100
		const targets = allocateEquityFloors(bn(700), [
			{ equity: bn(3000), buffer: bn(2900), currentFloor: bn(700) },
			{ equity: bn(1000), buffer: bn(50), currentFloor: ZERO },
		]);
		assert(targets !== null);
		assert(targets![0].add(targets![1]).eq(bn(700)));
		assert(targets![0].lte(bn(100)));
		assert(targets![1].lte(bn(950)));
	});

	it('returns null when the pool cannot back the floor plus buffers', () => {
		assert.isNull(
			allocateEquityFloors(bn(700), [
				{ equity: bn(300), buffer: bn(50), currentFloor: bn(700) },
				{ equity: bn(300), buffer: bn(50), currentFloor: ZERO },
			])
		);
	});

	it('pins entries in place and allocates around them', () => {
		const targets = allocateEquityFloors(
			bn(700),
			[
				{ equity: bn(100), buffer: bn(50), currentFloor: bn(200) },
				{ equity: bn(2000), buffer: bn(50), currentFloor: bn(500) },
				{ equity: bn(2000), buffer: bn(50), currentFloor: ZERO },
			],
			[true, false, false]
		);
		assert(targets !== null);
		assert(targets![0].eq(bn(200)), 'pinned floor moved');
		assert(targets![0].add(targets![1]).add(targets![2]).eq(bn(700)));
		assert(targets![1].eq(bn(250)));
		assert(targets![2].eq(bn(250)));
	});

	it('always conserves the total and respects capacity under fuzzing', () => {
		const rng = new Lcg(0xf100dcaf);
		for (let i = 0; i < 5_000; i++) {
			const n = 2 + rng.next(6);
			const subaccounts = Array.from({ length: n }, () => ({
				equity: bn(rng.next(1_000_000)),
				buffer: bn(rng.next(100_000)),
				currentFloor: bn(rng.next(300_000)),
			}));
			const totalFloor = bn(rng.next(1_500_000));
			const targets = allocateEquityFloors(totalFloor, subaccounts);
			if (targets === null) {
				const totalCap = subaccounts.reduce(
					(sum, s) => sum.add(BN.max(s.equity.sub(s.buffer), ZERO)),
					ZERO
				);
				assert(
					totalCap.lt(totalFloor),
					`null despite sufficient capacity at iteration ${i}`
				);
				continue;
			}
			const sum = targets.reduce((s, t) => s.add(t), ZERO);
			assert(sum.eq(totalFloor), `total drifted at iteration ${i}`);
			targets.forEach((t, j) => {
				assert(t.gte(ZERO), `negative target at iteration ${i}`);
				assert(
					t.lte(BN.max(subaccounts[j].equity.sub(subaccounts[j].buffer), ZERO)),
					`target above capacity at iteration ${i}`
				);
			});
		}
	});
});

describe('planFloorMoves', () => {
	it('produces moves that transform current into target and conserve the sum', () => {
		const rng = new Lcg(0xdeadbeef);
		for (let i = 0; i < 5_000; i++) {
			const n = 2 + rng.next(6);
			const ids = Array.from({ length: n }, (_, j) => j);
			const current = ids.map(() => bn(rng.next(1_000_000)));
			const total = current.reduce((s, c) => s.add(c), ZERO);
			// random target split of the same total
			const cuts = ids
				.slice(1)
				.map(() => (total.isZero() ? 0 : rng.next(total.toNumber() + 1)))
				.sort((a, b) => a - b);
			const target: BN[] = [];
			let prev = 0;
			for (const cut of cuts) {
				target.push(bn(cut - prev));
				prev = cut;
			}
			target.push(total.sub(bn(prev)));

			const moves = planFloorMoves(ids, current, target);
			const result = current.map((c) => c.clone());
			for (const move of moves) {
				result[move.fromSubAccountId] = result[move.fromSubAccountId].sub(
					move.equityFloorDelta
				);
				result[move.toSubAccountId] = result[move.toSubAccountId].add(
					move.equityFloorDelta
				);
				assert(
					move.equityFloorDelta.gt(ZERO),
					`zero-delta move at iteration ${i}`
				);
				assert(
					result[move.fromSubAccountId].gte(target[move.fromSubAccountId]),
					`source overshot below its target at iteration ${i}`
				);
			}
			result.forEach((r, j) => {
				assert(
					r.eq(target[j]),
					`result != target at iteration ${i} index ${j}`
				);
			});
		}
	});
});

describe('planCureMoves', () => {
	const cure = (
		subs: [number, number, number][], // [subAccountId, equityFloor, bufferedHeadroom]
		haircut = 1
	) =>
		planCureMoves(
			subs.map(([subAccountId, equityFloor, bufferedHeadroom]) => ({
				subAccountId,
				equityFloor: bn(equityFloor),
				bufferedHeadroom: bn(bufferedHeadroom),
			})),
			bn(haircut)
		);

	it('is a no-op when no subaccount is below its buffered floor', () => {
		assert.isEmpty(
			cure([
				[0, 480, 302],
				[1, 320, 5],
			])
		);
	});

	it('tops a deficit up to the haircut above its gate, funds only', () => {
		const plans = cure([
			[0, 480, 302],
			[1, 320, -22],
		]);
		assert.lengthOf(plans, 1);
		assert.equal(plans[0].fromSubAccountId, 0);
		assert.equal(plans[0].toSubAccountId, 1);
		assert(plans[0].amount.eq(bn(23))); // deficit 22 + haircut 1
		assert(plans[0].equityFloorDelta.eq(ZERO));
	});

	it('never targets a subaccount without a floor', () => {
		assert.isEmpty(
			cure([
				[0, 480, 302],
				[1, 0, -50],
			])
		);
	});

	it('lets a floorless subaccount donate', () => {
		const plans = cure([
			[0, 0, 200],
			[1, 320, -22],
		]);
		assert.lengthOf(plans, 1);
		assert.equal(plans[0].fromSubAccountId, 0);
		assert(plans[0].amount.eq(bn(23)));
	});

	it('combines donors to cure one deficit, largest donor first', () => {
		const plans = cure([
			[0, 100, 61], // cap 60
			[1, 320, -100], // deficit 101
			[2, 100, 51], // cap 50
		]);
		assert.lengthOf(plans, 2);
		assert.equal(plans[0].fromSubAccountId, 0);
		assert(plans[0].amount.eq(bn(60)));
		assert.equal(plans[1].fromSubAccountId, 2);
		assert(plans[1].amount.eq(bn(41)));
		assert(plans.every((p) => p.toSubAccountId === 1));
	});

	it('sends scarce donor equity to the deepest breach first', () => {
		const plans = cure([
			[0, 100, 11], // cap 10
			[1, 320, -50], // deficit 51, deepest
			[2, 320, -5], // deficit 6
		]);
		assert.lengthOf(plans, 1);
		assert.equal(plans[0].toSubAccountId, 1);
		assert(plans[0].amount.eq(bn(10))); // best-effort partial
	});

	it('never draws a donor below the haircut above its own gate', () => {
		const rng = new Lcg(0xc0ffee);
		for (let i = 0; i < 5_000; i++) {
			const n = 2 + rng.next(6);
			const subs: [number, number, number][] = Array.from(
				{ length: n },
				(_, j) => [
					j,
					rng.next(2) === 0 ? 0 : 1 + rng.next(1_000_000),
					rng.next(2_000_000) - 1_000_000,
				]
			);
			const headroom = subs.map(([, , h]) => bn(h));
			for (const plan of cure(subs)) {
				assert(plan.equityFloorDelta.eq(ZERO), `floor moved at ${i}`);
				assert(plan.amount.gt(ZERO), `zero-amount plan at ${i}`);
				headroom[plan.fromSubAccountId] = headroom[plan.fromSubAccountId].sub(
					plan.amount
				);
				headroom[plan.toSubAccountId] = headroom[plan.toSubAccountId].add(
					plan.amount
				);
				assert(
					headroom[plan.fromSubAccountId].gte(bn(1)),
					`donor drawn under its gate at ${i}`
				);
				assert(
					headroom[plan.toSubAccountId].lte(bn(1)),
					`deficit overfilled past haircut at ${i}`
				);
			}
		}
	});
});
