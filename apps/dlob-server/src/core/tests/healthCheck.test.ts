import {
	evaluateHealth,
	globalHealthState,
	HEALTH_CHECK_CONFIG,
	recordSlotDiffHealth,
	setHealthStatus,
	getHealthStatus,
	slotDiffUnhealthySince,
	HEALTH_STATUS,
} from '../healthCheck';

describe('healthCheck', () => {
	beforeEach(() => {
		globalHealthState.lastSlot = -1;
		globalHealthState.lastSlotTimestamp = Date.now();
		slotDiffUnhealthySince.clear();
		setHealthStatus(HEALTH_STATUS.Ok);
	});

	describe('cold start', () => {
		it('stays healthy while no poll has returned (slot 0)', () => {
			expect(evaluateHealth(0).isHealthy).toBe(true);
			// Well past MAX_SLOT_STALENESS_MS: a cold start must not trip liveness.
			globalHealthState.lastSlotTimestamp = Date.now() - 60_000;
			expect(evaluateHealth(0).isHealthy).toBe(true);
		});

		it('does not seed lastSlot from a pre-poll zero', () => {
			evaluateHealth(0);
			expect(globalHealthState.lastSlot).toBe(-1);
		});
	});

	describe('transient stall', () => {
		it('clears once slots advance again', () => {
			evaluateHealth(1000);
			// Stale long enough to be marked unhealthy.
			globalHealthState.lastSlotTimestamp =
				Date.now() - (HEALTH_CHECK_CONFIG.MAX_SLOT_STALENESS_MS + 1000);
			expect(evaluateHealth(1000).isHealthy).toBe(false);
			setHealthStatus(HEALTH_STATUS.UnhealthySlotSubscriber);

			// Chain recovers: a non-Restart status must not pin the pod unhealthy.
			expect(evaluateHealth(1100).isHealthy).toBe(true);
		});

		it('keeps Restart latched even when slots advance', () => {
			evaluateHealth(1000);
			setHealthStatus(HEALTH_STATUS.Restart);
			expect(evaluateHealth(1100).isHealthy).toBe(false);
			expect(evaluateHealth(1200).isHealthy).toBe(false);
		});
	});

	describe('kill-switch sustain window', () => {
		it('does not latch on a single bad sample', () => {
			recordSlotDiffHealth('SOL-PERP', true);
			expect(getHealthStatus()).toBe(HEALTH_STATUS.Ok);
		});

		it('does not latch when the gap recovers inside the window', () => {
			recordSlotDiffHealth('SOL-PERP', true);
			recordSlotDiffHealth('SOL-PERP', false);
			recordSlotDiffHealth('SOL-PERP', true);
			expect(getHealthStatus()).toBe(HEALTH_STATUS.Ok);
		});

		it('latches once the gap persists past the window', () => {
			recordSlotDiffHealth('SOL-PERP', true);
			slotDiffUnhealthySince.set(
				'SOL-PERP',
				Date.now() - (HEALTH_CHECK_CONFIG.KILL_SWITCH_SUSTAIN_MS + 1)
			);
			recordSlotDiffHealth('SOL-PERP', true);
			expect(getHealthStatus()).toBe(HEALTH_STATUS.Restart);
		});

		it('tracks markets independently', () => {
			recordSlotDiffHealth('SOL-PERP', true);
			slotDiffUnhealthySince.set(
				'SOL-PERP',
				Date.now() - (HEALTH_CHECK_CONFIG.KILL_SWITCH_SUSTAIN_MS + 1)
			);
			recordSlotDiffHealth('BTC-PERP', true);
			expect(slotDiffUnhealthySince.has('BTC-PERP')).toBe(true);
			expect(getHealthStatus()).toBe(HEALTH_STATUS.Ok);
		});
	});
});
