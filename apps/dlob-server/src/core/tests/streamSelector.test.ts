import {
	describe,
	expect,
	it,
	beforeEach,
	afterEach,
	jest,
} from '@jest/globals';
import {
	StreamSelector,
	STREAM_STALE_THRESHOLD_MS,
	STREAM_FROZEN_THRESHOLD_MS,
	STREAM_SWITCH_THRESHOLD_MS,
} from '../streamSelector';
import { GaugeValue } from '../metricsV2';

const A = 'dlob:';
const B = 'dlob-helius:';
const TICK_MS = 400;
const ticksFor = (ms: number) => Math.ceil(ms / TICK_MS) + 1;

const gauge = () => ({ setLatestValue: jest.fn() }) as unknown as GaugeValue;

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
		const frozenTicks = ticksFor(STREAM_FROZEN_THRESHOLD_MS);
		const forwarded = runTicks(frozenTicks + 5, {
			[A]: () => 100,
			[B]: (i) => 100 + i,
		});
		expect(selector.getActiveStream()).toBe(B);
		expect(forwarded[A]).toBeLessThanOrEqual(frozenTicks + 1);
		expect(forwarded[B]).toBeGreaterThanOrEqual(5);
	});

	it('fails over from an active feed that goes silent', () => {
		selector.recordMessage(A, 100);
		jest.advanceTimersByTime(STREAM_STALE_THRESHOLD_MS + 1);
		expect(selector.recordMessage(B, 100)).toBe(true);
		expect(selector.getActiveStream()).toBe(B);
	});

	it('tolerates slot steps slower than the silent window', () => {
		// Slot moves every 8 ticks (3.2s) while messages keep arriving every tick.
		const stepTicks = 8;
		const forwarded = runTicks(stepTicks * 6, {
			[A]: (i) => 100 + Math.floor(i / stepTicks),
			[B]: (i) => 100 + Math.floor(i / stepTicks),
		});
		expect(selector.getActiveStream()).toBe(A);
		expect(forwarded[A]).toBe(stepTicks * 6);
		expect(forwarded[B]).toBe(0);
	});

	it('switches to a feed that leads on slot for the switch threshold', () => {
		runTicks(ticksFor(STREAM_SWITCH_THRESHOLD_MS), {
			[A]: (i) => 100 + i,
			[B]: (i) => 105 + i,
		});
		expect(selector.getActiveStream()).toBe(B);
	});

	it('does not switch to a feed whose lead is intermittent', () => {
		runTicks(ticksFor(STREAM_SWITCH_THRESHOLD_MS) * 2, {
			[A]: (i) => 100 + i,
			// B is ahead on odd ticks only, so its lead never lasts.
			[B]: (i) => 100 + i + (i % 2),
		});
		expect(selector.getActiveStream()).toBe(A);
	});

	it('does not let a standby bank a lead across its own silence', () => {
		// B leads for two ticks, then goes silent while A keeps advancing.
		runTicks(2, { [A]: (i) => 100 + i, [B]: (i) => 110 + i });
		const silentTicks = ticksFor(STREAM_SWITCH_THRESHOLD_MS);
		for (let i = 2; i < 2 + silentTicks; i++) {
			selector.recordMessage(A, 100 + i);
			jest.advanceTimersByTime(TICK_MS);
		}
		// B returns with a single ahead slot: not a sustained lead.
		expect(selector.recordMessage(B, 200)).toBe(false);
		expect(selector.getActiveStream()).toBe(A);
	});

	it('does not switch to a standby frozen at a slot far ahead', () => {
		const forwarded = runTicks(ticksFor(STREAM_SWITCH_THRESHOLD_MS) * 2, {
			[A]: (i) => 100 + i,
			[B]: () => 10_000,
		});
		expect(selector.getActiveStream()).toBe(A);
		expect(forwarded[B]).toBe(0);
	});

	it('stays on the active feed when both feeds are frozen', () => {
		const ticks = ticksFor(STREAM_FROZEN_THRESHOLD_MS) + 5;
		const forwarded = runTicks(ticks, {
			[A]: () => 100,
			[B]: () => 100,
		});
		expect(selector.getActiveStream()).toBe(A);
		expect(forwarded[A]).toBe(ticks);
		expect(forwarded[B]).toBe(0);
	});

	it('checkHealth marks a frozen feed unhealthy and moves off it', () => {
		const healthy = gauge();
		selector = new StreamSelector([A, B], healthy, gauge());
		// Stop just before A counts as frozen, so recordMessage has not switched.
		const ticks = Math.floor(STREAM_FROZEN_THRESHOLD_MS / TICK_MS) - 1;
		runTicks(ticks, { [A]: () => 100, [B]: (i) => 100 + i });
		expect(selector.getActiveStream()).toBe(A);
		jest.advanceTimersByTime(
			STREAM_FROZEN_THRESHOLD_MS - ticks * TICK_MS + 100
		);
		jest.clearAllMocks();
		expect(selector.checkHealth()).toBe(true);
		expect(selector.getActiveStream()).toBe(B);
		expect(healthy.setLatestValue).toHaveBeenNthCalledWith(1, 0, { source: A });
		expect(healthy.setLatestValue).toHaveBeenNthCalledWith(2, 1, { source: B });
	});
});
