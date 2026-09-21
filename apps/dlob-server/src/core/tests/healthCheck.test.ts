import { describe, expect, it, beforeEach } from '@jest/globals';
import {
	evaluateHealth,
	globalHealthState,
	HEALTH_CHECK_CONFIG,
	positiveDurationMs,
	recordSlotDiffHealth,
	setHealthStatus,
	getHealthStatus,
	resetHealthState,
	slotDiffWindows,
	HEALTH_STATUS,
} from '../healthCheck';
import { handleHealthCheck } from '../middleware';
import { SlotSource } from '@velocity-exchange/sdk';

describe('healthCheck', () => {
	beforeEach(() => {
		resetHealthState();
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

	describe('latched Restart survives HTTP probes', () => {
		it('keeps returning 500 across consecutive probes', async () => {
			let slot = 1000;
			evaluateHealth(slot);
			setHealthStatus(HEALTH_STATUS.Restart);

			const codes: number[] = [];
			const res = { writeHead: (c: number) => codes.push(c), end: () => {} };
			const handler = handleHealthCheck({
				getSlot: () => slot,
			} as unknown as SlotSource);

			await handler({}, res, null);
			slot += 100;
			await handler({}, res, null);
			slot += 100;
			await handler({}, res, null);

			expect(codes).toEqual([500, 500, 500]);
		});

		it('is not cleared by a slot source dipping to 0', () => {
			evaluateHealth(1000);
			setHealthStatus(HEALTH_STATUS.Restart);
			expect(evaluateHealth(0).isHealthy).toBe(false);
			expect(getHealthStatus()).toBe(HEALTH_STATUS.Restart);
		});

		it('is not cleared before the first sample', () => {
			setHealthStatus(HEALTH_STATUS.Restart);
			expect(evaluateHealth(1000).isHealthy).toBe(false);
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
			const now = Date.now();
			slotDiffWindows.set('SOL-PERP', {
				since: now - (HEALTH_CHECK_CONFIG.KILL_SWITCH_SUSTAIN_MS + 1),
				lastSeen: now,
			});
			recordSlotDiffHealth('SOL-PERP', true);
			expect(getHealthStatus()).toBe(HEALTH_STATUS.Restart);
		});

		it('restarts the window when sampling itself stopped', () => {
			recordSlotDiffHealth('SOL-PERP', true);
			const now = Date.now();
			// Behind long ago, but not sampled since: elapsed time alone must not
			// count as having been behind the whole while.
			slotDiffWindows.set('SOL-PERP', {
				since: now - (HEALTH_CHECK_CONFIG.KILL_SWITCH_SUSTAIN_MS + 1),
				lastSeen: now - (HEALTH_CHECK_CONFIG.KILL_SWITCH_SAMPLE_GAP_MS + 1),
			});
			recordSlotDiffHealth('SOL-PERP', true);
			expect(getHealthStatus()).toBe(HEALTH_STATUS.Ok);
			expect(slotDiffWindows.get('SOL-PERP')?.since).toBeGreaterThan(
				now - HEALTH_CHECK_CONFIG.KILL_SWITCH_SAMPLE_GAP_MS
			);
		});

		it('tracks markets independently', () => {
			recordSlotDiffHealth('SOL-PERP', true);
			const now = Date.now();
			slotDiffWindows.set('SOL-PERP', {
				since: now - (HEALTH_CHECK_CONFIG.KILL_SWITCH_SUSTAIN_MS + 1),
				lastSeen: now,
			});
			recordSlotDiffHealth('BTC-PERP', true);
			expect(slotDiffWindows.has('BTC-PERP')).toBe(true);
			expect(getHealthStatus()).toBe(HEALTH_STATUS.Ok);
		});
	});

	describe('duration config validation', () => {
		it.each([
			['-5', 'negative'],
			['Infinity', 'infinite'],
			['abc', 'non-numeric'],
			['0', 'zero'],
			['', 'empty'],
			[undefined, 'unset'],
		])('falls back to the default for a %s override', (raw) => {
			expect(positiveDurationMs(raw, 42)).toBe(42);
		});

		it('accepts a finite positive override', () => {
			expect(positiveDurationMs('1500', 42)).toBe(1500);
		});

		it('ships the documented defaults', () => {
			expect(HEALTH_CHECK_CONFIG.KILL_SWITCH_SUSTAIN_MS).toBe(60_000);
			expect(HEALTH_CHECK_CONFIG.KILL_SWITCH_SAMPLE_GAP_MS).toBe(10_000);
			expect(HEALTH_CHECK_CONFIG.STARTUP_GRACE_MS).toBe(180_000);
		});
	});

	describe('startup grace', () => {
		it('goes unhealthy if no slot ever arrives', () => {
			globalHealthState.processStartedAt =
				Date.now() - (HEALTH_CHECK_CONFIG.STARTUP_GRACE_MS + 1000);
			const { isHealthy, reason } = evaluateHealth(0);
			expect(isHealthy).toBe(false);
			expect(reason).toContain('No slot after');
		});

		it('stays healthy at slot 0 inside the grace window', () => {
			globalHealthState.processStartedAt = Date.now();
			expect(evaluateHealth(0).isHealthy).toBe(true);
		});
	});
});
