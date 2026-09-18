export enum HEALTH_STATUS {
	Ok = 0,
	StaleBulkAccountLoader,
	UnhealthySlotSubscriber,
	LivenessTesting,
	Restart,
}

/**
 * These durations gate the restart safety mechanisms, so a bad override has to
 * fall back to the default rather than pass through. A plain `Number(x) || fb`
 * lets a negative or infinite value past. A negative sample gap resets the
 * window on every sample, and an infinite sustain never latches. Either one
 * turns the kill-switch off and reports nothing.
 */
function positiveDurationMs(
	value: string | undefined,
	fallback: number
): number {
	const parsed = Number(value);
	return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

/**
 * Health check configuration
 */
const HEALTH_CHECK_CONFIG = {
	CHECK_INTERVAL_MS: 2000,
	// Maximum time allowed between slot updates
	MAX_SLOT_STALENESS_MS: parseInt(process.env.MAX_SLOT_STALENESS_MS || '5000'),
	// Minimum expected slot advancement rate. Sometimes usermap is used for slot source, so the slot rate
	// may be much lower than 2 per sec. 0.03 is 1 slot per 33s
	MIN_SLOT_RATE: parseFloat(process.env.MIN_SLOT_RATE || '0.03'),
	// How long the DLOB slot must stay behind the oracle before the kill-switch
	// latches. The check samples several times a second, so without a window one
	// bad sample would restart the pod.
	KILL_SWITCH_SUSTAIN_MS: positiveDurationMs(
		process.env.KILL_SWITCH_SUSTAIN_MS,
		60_000
	),
	// If sampling stops, elapsed time says nothing about whether the market was
	// behind for the whole period. A gap this long restarts the window.
	KILL_SWITCH_SAMPLE_GAP_MS: positiveDurationMs(
		process.env.KILL_SWITCH_SAMPLE_GAP_MS,
		10_000
	),
	// How long a process may report no slot at all before it counts as stuck.
	// Without a bound, a slot source that never delivers stays liveness-healthy
	// for good, which is worse than a cold-start crash loop.
	STARTUP_GRACE_MS: positiveDurationMs(process.env.STARTUP_GRACE_MS, 180_000),
};

/**
 * Tracks the health state of the slot subscriber
 */
type HealthState = {
	lastSlot: number;
	lastSlotTimestamp: number;
	processStartedAt: number;
};

const globalHealthState: HealthState = {
	lastSlot: -1,
	lastSlotTimestamp: Date.now(),
	processStartedAt: Date.now(),
};

/**
 * Evaluates if the current state is healthy based on slot progression
 */
function evaluateHealth(currentSlot: number): {
	isHealthy: boolean;
	reason?: string;
} {
	const now = Date.now();

	// Restart is the kill-switch and stays latched until the pod is replaced, so
	// it outranks every other branch here. A slot source that dips to 0 on
	// reconnect must not return a healthy verdict and clear it. Every other
	// status is derived again from slot progression below, so a short stall
	// clears itself instead of holding the pod unhealthy.
	if (getHealthStatus() === HEALTH_STATUS.Restart) {
		return {
			isHealthy: false,
			reason: `Unhealthy state: ${HEALTH_STATUS.Restart}`,
		};
	}

	// Slot 0 means no poll has returned yet. A publisher starting against an
	// empty book stays here until its first poll lands. Calling that unhealthy
	// crash-loops a cold start, which is why the mainnet manifests moved to TCP
	// probes. Tolerate slot 0 only for the startup window. A slot source that
	// never delivers one slot is stuck rather than starting.
	if (currentSlot === 0) {
		const sinceStart = now - globalHealthState.processStartedAt;
		if (sinceStart < HEALTH_CHECK_CONFIG.STARTUP_GRACE_MS) {
			return { isHealthy: true };
		}
		return {
			isHealthy: false,
			reason: `No slot after ${sinceStart}ms (startup grace ${HEALTH_CHECK_CONFIG.STARTUP_GRACE_MS}ms)`,
		};
	}

	// First health check
	if (globalHealthState.lastSlot === -1) {
		globalHealthState.lastSlot = currentSlot;
		globalHealthState.lastSlotTimestamp = now;
		return { isHealthy: true };
	}

	const timeDelta = now - globalHealthState.lastSlotTimestamp;
	const slotDelta = currentSlot - globalHealthState.lastSlot;

	// If slot has progressed, we are healthy, check rate and update state.
	if (currentSlot > globalHealthState.lastSlot) {
		// Update state
		globalHealthState.lastSlot = currentSlot;
		globalHealthState.lastSlotTimestamp = now;

		// Check if slot update rate is too low
		const slotRate = (slotDelta / timeDelta) * 1000; // Convert to per second
		if (slotRate < HEALTH_CHECK_CONFIG.MIN_SLOT_RATE) {
			return {
				isHealthy: false,
				reason: `Slot update rate ${slotRate.toFixed(
					2
				)} slots/sec below minimum ${HEALTH_CHECK_CONFIG.MIN_SLOT_RATE}`,
			};
		}

		return { isHealthy: true };
	}

	// If slot has NOT progressed, check for staleness.
	if (timeDelta > HEALTH_CHECK_CONFIG.MAX_SLOT_STALENESS_MS) {
		return {
			isHealthy: false,
			reason: `No slot updates in ${timeDelta}ms (max ${HEALTH_CHECK_CONFIG.MAX_SLOT_STALENESS_MS}ms)`,
		};
	}

	// Slot has not progressed, but not stale yet. Still healthy.
	return { isHealthy: true };
}

/**
 * Tracks, per market, when the DLOB slot first fell behind the oracle and when
 * it was last sampled. The kill-switch latches only once a market has been
 * behind continuously for KILL_SWITCH_SUSTAIN_MS, so a brief oracle or RPC
 * hiccup cannot restart a pod that is otherwise serving fine.
 *
 * lastSeen is what makes "continuously" true. Elapsed time alone is also
 * satisfied by one bad sample, a long gap in sampling, and then a second bad
 * sample. A stalled publisher is the case that samples least often, so that gap
 * is real rather than hypothetical.
 *
 * The window is process-wide, so two publishers over the same market share one
 * window. They read the same slot source and oracle data, so they agree in
 * practice.
 */
type SlotDiffWindow = { since: number; lastSeen: number };
const slotDiffWindows: Map<string, SlotDiffWindow> = new Map();

function recordSlotDiffHealth(marketName: string, isBehind: boolean): void {
	if (!isBehind) {
		slotDiffWindows.delete(marketName);
		return;
	}

	const now = Date.now();
	const window = slotDiffWindows.get(marketName);
	if (
		window === undefined ||
		now - window.lastSeen > HEALTH_CHECK_CONFIG.KILL_SWITCH_SAMPLE_GAP_MS
	) {
		slotDiffWindows.set(marketName, { since: now, lastSeen: now });
		return;
	}

	window.lastSeen = now;
	const behindFor = now - window.since;
	if (behindFor >= HEALTH_CHECK_CONFIG.KILL_SWITCH_SUSTAIN_MS) {
		console.log(
			`Kill-switch: ${marketName} has been behind the oracle for ${behindFor}ms, flagging process for restart`
		);
		setHealthStatus(HEALTH_STATUS.Restart);
	}
}

/** Test seam: clears every piece of module-global health state. */
function resetHealthState(): void {
	globalHealthState.lastSlot = -1;
	globalHealthState.lastSlotTimestamp = Date.now();
	globalHealthState.processStartedAt = Date.now();
	slotDiffWindows.clear();
	setHealthStatus(HEALTH_STATUS.Ok);
}

let healthStatus: HEALTH_STATUS = HEALTH_STATUS.Ok;
const setHealthStatus = (status: HEALTH_STATUS): void => {
	healthStatus = status;
};

const getHealthStatus = (): HEALTH_STATUS => {
	return healthStatus;
};

export {
	evaluateHealth,
	globalHealthState,
	HEALTH_CHECK_CONFIG,
	recordSlotDiffHealth,
	resetHealthState,
	setHealthStatus,
	getHealthStatus,
	slotDiffWindows,
};
