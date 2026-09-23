import { GaugeValue } from './metricsV2';

// Stream selector with hysteresis to prevent flip-flopping between sources
export const STREAM_SWITCH_THRESHOLD_MS = 10_000; // 10 seconds
export const STREAM_STALE_THRESHOLD_MS = 3_000; // Consider stream stale if its slot has not advanced for 3 seconds

export class StreamSelector {
	private activeStream: string | null = null;
	private streamLastMessageTime: Map<string, number> = new Map();
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

	// Record a message from a stream and return whether it should be forwarded
	recordMessage(stream: string, slot: number): boolean {
		const now = Date.now();
		// A feed whose upstream froze keeps publishing the same slot every tick,
		// so a stream only counts as alive when its slot moves forward.
		if (slot > (this.streamLastSlot.get(stream) ?? -1)) {
			this.streamLastMessageTime.set(stream, now);
			this.streamLastSlot.set(stream, slot);
			this.streamHealthyGauge.setLatestValue(1, { source: stream });
		}

		if (this.activeStream === null) {
			this.setActiveStream(stream);
			return true;
		}

		// If this is from the active stream, forward it
		if (stream === this.activeStream) {
			return true;
		}

		// This message is from a non-active stream
		const activeSlot = this.streamLastSlot.get(this.activeStream) || 0;
		const activeLastMessage =
			this.streamLastMessageTime.get(this.activeStream) || 0;

		// Check if active stream is stale
		if (now - activeLastMessage > STREAM_STALE_THRESHOLD_MS) {
			console.log(
				`Active stream ${this.activeStream} is stale (no message for ${
					now - activeLastMessage
				}ms), switching to ${stream}`
			);
			this.setActiveStream(stream);
			return true;
		}

		// Check if this stream has a more recent slot
		if (slot > activeSlot) {
			// Track how long this stream has been leading
			if (!this.streamLeadingSince.has(stream)) {
				this.streamLeadingSince.set(stream, now);
			}

			const leadingDuration = now - this.streamLeadingSince.get(stream)!;
			if (leadingDuration >= STREAM_SWITCH_THRESHOLD_MS) {
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
			const lastMessage = this.streamLastMessageTime.get(stream);
			const isHealthy =
				lastMessage && now - lastMessage < STREAM_STALE_THRESHOLD_MS;

			this.streamHealthyGauge.setLatestValue(isHealthy ? 1 : 0, {
				source: stream,
			});

			if (isHealthy) {
				anyHealthy = true;
			}
		}

		// If active stream is no longer healthy, try to switch
		if (this.activeStream) {
			const activeLastMessage = this.streamLastMessageTime.get(
				this.activeStream
			);
			if (
				!activeLastMessage ||
				now - activeLastMessage > STREAM_STALE_THRESHOLD_MS
			) {
				// Find a healthy stream to switch to
				for (const stream of this.streams) {
					const lastMessage = this.streamLastMessageTime.get(stream);
					if (lastMessage && now - lastMessage < STREAM_STALE_THRESHOLD_MS) {
						console.log(
							`Active stream ${this.activeStream} is unhealthy, switching to ${stream}`
						);
						this.setActiveStream(stream);
						break;
					}
				}
			}
		}

		return anyHealthy;
	}
}
