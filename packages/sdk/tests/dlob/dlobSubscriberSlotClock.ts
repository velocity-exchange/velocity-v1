import { assert } from 'chai';
import { BN, DLOBSubscriber, MarketType, StateAccount } from '../../src';
import * as orderBookLevels from '../../src/dlob/orderBookLevels';
import { mockPerpMarkets, mockStateAccount } from './helpers';

/**
 * The vAMM quote's slot duration must be resolved at the slot the quote is
 * priced at. `getVammL2Generator` prices at `latestSlot` when the caller
 * supplies one, so resolving at the subscription slot instead would measure
 * elapsed slots in the post-flip regime and convert them at the pre-flip
 * duration — an inconsistent quote across a gate boundary.
 */
describe('DLOBSubscriber vAMM slot-duration consistency', () => {
	const EFFECTIVE_SLOT = 1_000;
	const original = orderBookLevels.getVammL2Generator;
	let seen: { slotDuration?: number; latestSlot?: BN };

	beforeEach(() => {
		seen = {};
		// Direct-import call sites compile to a property access on the module
		// object, so replacing the export intercepts DLOBSubscriber's call.
		Object.defineProperty(orderBookLevels, 'getVammL2Generator', {
			configurable: true,
			writable: true,
			value: (args: { slotDuration: number; latestSlot?: BN }) => {
				seen = { slotDuration: args.slotDuration, latestSlot: args.latestSlot };
				return { getL2Levels: () => [] };
			},
		});
	});

	afterEach(() => {
		Object.defineProperty(orderBookLevels, 'getVammL2Generator', {
			configurable: true,
			writable: true,
			value: original,
		});
	});

	const resolveAt = (subscriptionSlot: number, latestSlot?: BN) => {
		const state: StateAccount = {
			...mockStateAccount,
			slotDurationMs: 400,
			pendingSlotDurationMs: 200,
			slotDurationEffectiveSlot: new BN(EFFECTIVE_SLOT),
		};
		const velocityClient = {
			getStateAccount: () => state,
			getPerpMarketAccountOrThrow: () => mockPerpMarkets[0],
			getMMOracleDataForPerpMarket: () => ({
				price: new BN(0),
				slot: new BN(subscriptionSlot),
				confidence: new BN(0),
				hasSufficientNumberOfDataPoints: true,
			}),
		};
		const subscriber = new DLOBSubscriber({
			velocityClient: velocityClient as never,
			dlobSource: { getDLOB: async () => undefined as never },
			slotSource: { getSlot: () => subscriptionSlot },
			updateFrequency: 1_000,
		} as never);
		(subscriber as unknown as { dlob: unknown }).dlob = {
			getL2: () => ({ bids: [], asks: [], slot: subscriptionSlot }),
		};

		subscriber.getL2({
			marketIndex: 0,
			marketType: MarketType.PERP,
			depth: 1,
			includeVamm: true,
			latestSlot,
		});
		return seen;
	};

	it('resolves the post-flip duration when latestSlot has crossed the gate', () => {
		// Subscription slot is pre-flip; the quote is priced one slot later, on
		// the effective slot. Resolving at the subscription slot would hand the
		// generator 400ms for a quote it prices in the 200ms regime.
		const got = resolveAt(EFFECTIVE_SLOT - 1, new BN(EFFECTIVE_SLOT));
		assert.equal(got.latestSlot?.toNumber(), EFFECTIVE_SLOT);
		assert.equal(got.slotDuration, 200);
	});

	it('resolves the pre-flip duration when latestSlot is still short of the gate', () => {
		const got = resolveAt(EFFECTIVE_SLOT - 5, new BN(EFFECTIVE_SLOT - 1));
		assert.equal(got.slotDuration, 400);
	});

	it('falls back to the subscription slot when no latestSlot is given', () => {
		// latestSlot undefined means the generator does not use slotDuration at
		// all, but the value must still be the live one, not a stale guess.
		const got = resolveAt(EFFECTIVE_SLOT + 1, undefined);
		assert.isUndefined(got.latestSlot);
		assert.equal(got.slotDuration, 200);
	});

	it('reports the baseline when the slot feed is dead', () => {
		const got = resolveAt(0, undefined);
		assert.equal(got.slotDuration, 400);
	});
});
