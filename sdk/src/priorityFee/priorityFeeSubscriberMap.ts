import {
	DriftMarketInfo,
	VelocityPriorityFeeLevels,
	VelocityPriorityFeeResponse,
	fetchDriftPriorityFee,
} from './driftPriorityFeeMethod';
import {
	DEFAULT_PRIORITY_FEE_MAP_FREQUENCY_MS,
	PriorityFeeSubscriberMapConfig,
} from './types';

/**
 * takes advantage of /batchPriorityFees endpoint from drift hosted priority fee service
 */
export class PriorityFeeSubscriberMap {
	frequencyMs: number;
	intervalId?: ReturnType<typeof setTimeout>;

	driftMarkets?: DriftMarketInfo[];
	velocityPriorityFeeEndpoint?: string;
	/** @deprecated Use `velocityPriorityFeeEndpoint` instead. `driftPriorityFeeEndpoint` will be removed in a future major. */
	public get driftPriorityFeeEndpoint(): string | undefined {
		return this.velocityPriorityFeeEndpoint;
	}
	feesMap: Map<string, Map<number, VelocityPriorityFeeLevels>>; // marketType -> marketIndex -> priority fee

	public constructor(config: PriorityFeeSubscriberMapConfig) {
		this.frequencyMs = config.frequencyMs;
		this.frequencyMs =
			config.frequencyMs ?? DEFAULT_PRIORITY_FEE_MAP_FREQUENCY_MS;
		const endpoint =
			config.velocityPriorityFeeEndpoint ?? config.driftPriorityFeeEndpoint;
		if (!endpoint) {
			throw new Error(
				'PriorityFeeSubscriberMap: velocityPriorityFeeEndpoint (or deprecated driftPriorityFeeEndpoint) must be provided'
			);
		}
		this.velocityPriorityFeeEndpoint = endpoint;
		this.driftMarkets = config.driftMarkets;
		this.feesMap = new Map<string, Map<number, VelocityPriorityFeeLevels>>();
		this.feesMap.set('perp', new Map<number, VelocityPriorityFeeLevels>());
		this.feesMap.set('spot', new Map<number, VelocityPriorityFeeLevels>());
	}

	private updateFeesMap(
		velocityPriorityFeeResponse: VelocityPriorityFeeResponse
	) {
		velocityPriorityFeeResponse.forEach((fee: VelocityPriorityFeeLevels) => {
			this.feesMap.get(fee.marketType)!.set(fee.marketIndex, fee);
		});
	}

	public async subscribe(): Promise<void> {
		if (this.intervalId) {
			return;
		}

		await this.load();
		this.intervalId = setInterval(this.load.bind(this), this.frequencyMs);
	}

	public async unsubscribe(): Promise<void> {
		if (this.intervalId) {
			clearInterval(this.intervalId);
			this.intervalId = undefined;
		}
	}

	public async load(): Promise<void> {
		try {
			if (!this.driftMarkets) {
				return;
			}
			const fees = await fetchDriftPriorityFee(
				this.velocityPriorityFeeEndpoint!,
				this.driftMarkets.map((m) => m.marketType),
				this.driftMarkets.map((m) => m.marketIndex)
			);
			this.updateFeesMap(fees);
		} catch (e) {
			console.error('Error fetching drift priority fees', e);
		}
	}

	public updateMarketTypeAndIndex(driftMarkets: DriftMarketInfo[]) {
		this.driftMarkets = driftMarkets;
	}

	public getPriorityFees(
		marketType: string,
		marketIndex: number
	): VelocityPriorityFeeLevels | undefined {
		return this.feesMap.get(marketType)?.get(marketIndex);
	}
}

/** Example usage:
async function main() {
    const driftMarkets: DriftMarketInfo[] = [
        { marketType: 'perp', marketIndex: 0 },
        { marketType: 'perp', marketIndex: 1 },
        { marketType: 'spot', marketIndex: 2 }
    ];

    const subscriber = new PriorityFeeSubscriberMap({
        driftPriorityFeeEndpoint: 'https://dlob.drift.trade',
        frequencyMs: 5000,
        driftMarkets
    });
    await subscriber.subscribe();

    for (let i = 0; i < 20; i++) {
        await new Promise(resolve => setTimeout(resolve, 1000));
        driftMarkets.forEach(market => {
            const fees = subscriber.getPriorityFees(market.marketType, market.marketIndex);
            console.log(`Priority fees for ${market.marketType} market ${market.marketIndex}:`, fees);
        });
    }


    await subscriber.unsubscribe();
}

main().catch(console.error);
*/
