import { GaugeValue } from './metricsV2';

// Stream selector with hysteresis to prevent flip-flopping between sources
export const STREAM_SWITCH_THRESHOLD_MS = 10_000; // a standby must lead for this long to take over
export const STREAM_STALE_THRESHOLD_MS = 3_000; // no message at all for this long
// A publisher whose RPC froze keeps publishing the same slot every tick, so
// "alive" also needs the slot to move. Production publishers take their slot
// from getProgramAccounts polling, which can take a few seconds per cycle,
// hence the wider window.
export const STREAM_FROZEN_THRESHOLD_MS = 10_000;

export class StreamSelector {
	private activeStream: string | null = null;
	private streamLastMessageTime: Map<string, number> = new Map();
	private streamLastProgressTime: Map<string, number> = new Map();
	private streamLastSlot: Map<string, number> = new Map();
	private streamLeadingSince: Map<string, number> = new Map();
	private streamHealthyGauge: GaugeValue;
	private activeStreamGauge: GaugeValue;
	private streams: string[];

	constructor(
		streams: string[],
		streamHealthyGauge: GaugeValue,
		activeStreamGauge: GaugeValue
	) {
		this.streams = streams;
		this.streamHealthyGauge = streamHealthyGauge;
		this.activeStreamGauge = activeStreamGauge;

		// Initialize all streams as unhealthy
		for (const stream of streams) {
			this.streamHealthyGauge.setLatestValue(0, { source: stream });
			this.activeStreamGauge.setLatestValue(0, { source: stream });
		}
	}

	private isStale(stream: string, now: number): boolean {
		const lastMessage = this.streamLastMessageTime.get(stream);
		const lastProgress = this.streamLastProgressTime.get(stream);
		return (
			!lastMessage ||
			now - lastMessage >= STREAM_STALE_THRESHOLD_MS ||
			!lastProgress ||
			now - lastProgress >= STREAM_FROZEN_THRESHOLD_MS
		);
	}

	private staleReason(stream: string, now: number): string {
		const sinceMessage = now - (this.streamLastMessageTime.get(stream) ?? 0);
		const sinceProgress = now - (this.streamLastProgressTime.get(stream) ?? 0);
		return sinceMessage >= STREAM_STALE_THRESHOLD_MS
			? `no message for ${sinceMessage}ms`
			: `slot not advanced for ${sinceProgress}ms`;
	}

	// Record a message from a stream and return whether it should be forwarded
	recordMessage(stream: string, slot: number): boolean {
		const now = Date.now();
		const wasStale = this.isStale(stream, now);

		this.streamLastMessageTime.set(stream, now);
		if (slot > (this.streamLastSlot.get(stream) ?? -1)) {
			this.streamLastSlot.set(stream, slot);
			this.streamLastProgressTime.set(stream, now);
		}
		this.streamHealthyGauge.setLatestValue(this.isStale(stream, now) ? 0 : 1, {
			source: stream,
		});

		if (this.activeStream === null) {
			this.setActiveStream(stream);
			return true;
		}

		// If this is from the active stream, forward it
		if (stream === this.activeStream) {
			return true;
		}

		// This message is from a non-active stream. Switch to it only if the
		// active stream is stale and this one is not, or two frozen feeds would
		// flip on every message.
		if (this.isStale(this.activeStream, now) && !this.isStale(stream, now)) {
			console.log(
				`Active stream ${this.activeStream} is stale (${this.staleReason(
					this.activeStream,
					now
				)}), switching to ${stream}`
			);
			this.setActiveStream(stream);
			return true;
		}

		// Check if this stream has a more recent slot
		const activeSlot = this.streamLastSlot.get(this.activeStream) || 0;
		if (slot > activeSlot) {
			// Track how long this stream has been leading. A lead only counts
			// while the stream stays fresh, so a silent or frozen standby cannot
			// bank a lead and cash it in later.
			if (!this.streamLeadingSince.has(stream) || wasStale) {
				this.streamLeadingSince.set(stream, now);
			}

			const leadingDuration = now - this.streamLeadingSince.get(stream)!;
			if (
				leadingDuration >= STREAM_SWITCH_THRESHOLD_MS &&
				!this.isStale(stream, now)
			) {
				console.log(
					`Stream ${stream} has been leading for ${leadingDuration}ms, switching from ${this.activeStream}`
				);
				this.setActiveStream(stream);
				return true;
			}
		} else {
			// This stream is not leading, reset its leading time
			this.streamLeadingSince.delete(stream);
		}

		// Don't forward messages from non-active streams that haven't proven themselves
		return false;
	}

	private setActiveStream(stream: string) {
		if (this.activeStream) {
			this.activeStreamGauge.setLatestValue(0, { source: this.activeStream });
		}

		this.activeStream = stream;
		this.activeStreamGauge.setLatestValue(1, { source: stream });
		this.streamLeadingSince.clear();

		console.log(`Active stream set to: ${stream}`);
	}

	getActiveStream(): string | null {
		return this.activeStream;
	}

	checkHealth() {
		const now = Date.now();
		let anyHealthy = false;

		for (const stream of this.streams) {
			const isHealthy = !this.isStale(stream, now);
			this.streamHealthyGauge.setLatestValue(isHealthy ? 1 : 0, {
				source: stream,
			});
			if (isHealthy) {
				anyHealthy = true;
			}
		}

		// If active stream is no longer healthy, try to switch
		if (this.activeStream && this.isStale(this.activeStream, now)) {
			for (const stream of this.streams) {
				if (!this.isStale(stream, now)) {
					console.log(
						`Active stream ${
							this.activeStream
						} is unhealthy (${this.staleReason(
							this.activeStream,
							now
						)}), switching to ${stream}`
					);
					this.setActiveStream(stream);
					break;
				}
			}
		}

		return anyHealthy;
	}
}
