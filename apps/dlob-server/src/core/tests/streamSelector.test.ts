import { describe, expect, it, beforeEach, afterEach, jest } from '@jest/globals';
import {
	StreamSelector,
	STREAM_STALE_THRESHOLD_MS,
	STREAM_SWITCH_THRESHOLD_MS,
} from '../streamSelector';
import { GaugeValue } from '../metricsV2';

const A = 'dlob:';
const B = 'dlob-helius:';
const TICK_MS = 400;

const gauge = () =>
	({ setLatestValue: jest.fn() } as unknown as GaugeValue);

describe('StreamSelector', () => {
	let selector: StreamSelector;

	beforeEach(() => {
		jest.useFakeTimers();
		jest.setSystemTime(1_000_000);
		selector = new StreamSelector([A, B], gauge(), gauge());
	});

	afterEach(() => {
		jest.useRealTimers();
	});

	// Both feeds publish once per tick. `slotOf` gives each feed's slot at tick i.
	const runTicks = (
		ticks: number,
		slotOf: Record<string, (i: number) => number>
	) => {
		const forwarded: Record<string, number> = { [A]: 0, [B]: 0 };
		for (let i = 0; i < ticks; i++) {
			for (const stream of [A, B]) {
				if (selector.recordMessage(stream, slotOf[stream](i))) {
					forwarded[stream]++;
				}
			}
			jest.advanceTimersByTime(TICK_MS);
		}
		return forwarded;
	};

	it('forwards only the first feed to send while both advance together', () => {
		const forwarded = runTicks(50, {
			[A]: (i) => 100 + i,
			[B]: (i) => 100 + i,
		});
		expect(selector.getActiveStream()).toBe(A);
		expect(forwarded[A]).toBe(50);
		expect(forwarded[B]).toBe(0);
	});

	it('fails over from an active feed that keeps publishing a frozen slot', () => {
		const staleTicks = Math.ceil(STREAM_STALE_THRESHOLD_MS / TICK_MS) + 1;
		const forwarded = runTicks(staleTicks + 5, {
			[A]: () => 100,
			[B]: (i) => 100 + i,
		});
		expect(selector.getActiveStream()).toBe(B);
		// A is dropped once B takes over, and B's messages get through.
		expect(forwarded[A]).toBeLessThanOrEqual(staleTicks);
		expect(forwarded[B]).toBeGreaterThanOrEqual(5);
	});

	it('fails over from an active feed that goes silent', () => {
		selector.recordMessage(A, 100);
		jest.advanceTimersByTime(STREAM_STALE_THRESHOLD_MS + 1);
		expect(selector.recordMessage(B, 100)).toBe(true);
		expect(selector.getActiveStream()).toBe(B);
	});

	it('switches to a feed that leads on slot for the switch threshold', () => {
		const leadTicks = Math.ceil(STREAM_SWITCH_THRESHOLD_MS / TICK_MS) + 1;
		runTicks(leadTicks, {
			[A]: (i) => 100 + i,
			[B]: (i) => 105 + i,
		});
		expect(selector.getActiveStream()).toBe(B);
	});

	it('does not switch to a feed whose lead is intermittent', () => {
		const leadTicks = Math.ceil(STREAM_SWITCH_THRESHOLD_MS / TICK_MS) + 1;
		runTicks(leadTicks * 2, {
			[A]: (i) => 100 + i,
			// B is ahead on odd ticks only, so its lead never lasts.
			[B]: (i) => 100 + i + (i % 2),
		});
		expect(selector.getActiveStream()).toBe(A);
	});

	it('checkHealth marks a frozen feed unhealthy and moves off it', () => {
		selector.recordMessage(A, 100);
		selector.recordMessage(B, 100);
		for (let i = 1; i <= 10; i++) {
			jest.advanceTimersByTime(TICK_MS);
			selector.recordMessage(A, 100);
			selector.recordMessage(B, 100 + i);
		}
		expect(selector.checkHealth()).toBe(true);
		expect(selector.getActiveStream()).toBe(B);
	});
});
