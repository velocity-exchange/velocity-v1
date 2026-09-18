import { assert } from 'chai';
import {
	activeSlotDurationFromState,
	BN,
	DLOBSubscriber,
	MarketType,
	StateAccount,
} from '../../src';
import * as orderBookLevels from '../../src/dlob/orderBookLevels';
import { mockPerpMarkets, mockStateAccount } from './helpers';

/**
 * The vAMM quote must receive the full State clock so smoothing can integrate
 * an interval crossing a slot-duration boundary.
 */
describe('DLOBSubscriber vAMM slot-duration consistency', () => {
	const EFFECTIVE_SLOT = 1_000;
	const original = orderBookLevels.getVammL2Generator;
	let seen: { slotDurationState?: StateAccount; latestSlot?: BN };

	beforeEach(() => {
		seen = {};
		// Direct-import call sites compile to a property access on the module
		// object, so replacing the export intercepts DLOBSubscriber's call.
		Object.defineProperty(orderBookLevels, 'getVammL2Generator', {
			configurable: true,
			writable: true,
			value: (args: { slotDurationState: StateAccount; latestSlot?: BN }) => {
				seen = {
					slotDurationState: args.slotDurationState,
					latestSlot: args.latestSlot,
				};
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
		assert.equal(
			activeSlotDurationFromState(
				got.slotDurationState!,
				new BN(EFFECTIVE_SLOT)
			),
			200
		);
	});

	it('resolves the pre-flip duration when latestSlot is still short of the gate', () => {
		const got = resolveAt(EFFECTIVE_SLOT - 5, new BN(EFFECTIVE_SLOT - 1));
		assert.equal(
			activeSlotDurationFromState(
				got.slotDurationState!,
				new BN(EFFECTIVE_SLOT - 1)
			),
			400
		);
	});

	it('falls back to the subscription slot when no latestSlot is given', () => {
		const got = resolveAt(EFFECTIVE_SLOT + 1, undefined);
		assert.isUndefined(got.latestSlot);
		assert.equal(got.slotDurationState?.pendingSlotDurationMs, 200);
	});
});
