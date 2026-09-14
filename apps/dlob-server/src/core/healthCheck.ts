export enum HEALTH_STATUS {
	Ok = 0,
	StaleBulkAccountLoader,
	UnhealthySlotSubscriber,
	LivenessTesting,
	Restart,
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
	// latches. The check samples several times a second, so without a window a
	// single bad sample would restart the pod.
	KILL_SWITCH_SUSTAIN_MS: parseInt(
		process.env.KILL_SWITCH_SUSTAIN_MS || '60000'
	),
};

/**
 * Tracks the health state of the slot subscriber
 */
type HealthState = {
	lastSlot: number;
	lastSlotTimestamp: number;
};

const globalHealthState: HealthState = {
	lastSlot: -1,
	lastSlotTimestamp: Date.now(),
};

/**
 * Evaluates if the current state is healthy based on slot progression
 */
function evaluateHealth(currentSlot: number): {
	isHealthy: boolean;
	reason?: string;
} {
	const now = Date.now();

	// Slot 0 means no poll has returned yet, so there is nothing to judge. A
	// publisher starting against an empty book sits here until its first poll
	// lands; calling that unhealthy is what crash-looped cold starts and forced
	// the mainnet manifests onto TCP probes.
	if (currentSlot === 0) {
		return { isHealthy: true };
	}

	// First health check
	if (globalHealthState.lastSlot === -1) {
		globalHealthState.lastSlot = currentSlot;
		globalHealthState.lastSlotTimestamp = now;
		return { isHealthy: true };
	}

	// Restart is the deliberate kill-switch and stays latched until the pod is
	// replaced. Every other status is re-derived from slot progression below, so
	// a transient stall clears itself instead of pinning the pod unhealthy.
	if (getHealthStatus() === HEALTH_STATUS.Restart) {
		return {
			isHealthy: false,
			reason: `Unhealthy state: ${HEALTH_STATUS.Restart}`,
		};
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
 * Tracks, per market, when the DLOB slot first fell behind the oracle. The
 * kill-switch latches only once a market has been behind continuously for
 * KILL_SWITCH_SUSTAIN_MS, so a brief oracle or RPC hiccup cannot restart a pod
 * that is otherwise serving fine.
 */
const slotDiffUnhealthySince: Map<string, number> = new Map();

function recordSlotDiffHealth(marketName: string, isBehind: boolean): void {
	if (!isBehind) {
		slotDiffUnhealthySince.delete(marketName);
		return;
	}

	const now = Date.now();
	const since = slotDiffUnhealthySince.get(marketName);
	if (since === undefined) {
		slotDiffUnhealthySince.set(marketName, now);
		return;
	}

	if (now - since >= HEALTH_CHECK_CONFIG.KILL_SWITCH_SUSTAIN_MS) {
		console.log(
			`Kill-switch: ${marketName} has been behind the oracle for ${
				now - since
			}ms, flagging process for restart`
		);
		setHealthStatus(HEALTH_STATUS.Restart);
	}
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
	setHealthStatus,
	getHealthStatus,
	slotDiffUnhealthySince,
};
