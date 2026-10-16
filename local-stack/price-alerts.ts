// PRICE_ALERT messages in the shape of the data pipeline's oracle-feed, which a candle processor
// reads to move a candle's oracle price between fills.
import { PublishCommand, SNSClient } from '@aws-sdk/client-sns';

export type OraclePrice = {
	symbol: string;
	oracle: string;
	price: number;
	confidence: number;
};

export class PriceAlertPublisher {
	private readonly client: SNSClient;
	private readonly lastPrices = new Map<string, number>();

	constructor(private readonly fifoTopicArn: string) {
		const region = fifoTopicArn.split(':')[3];
		this.client = new SNSClient({ region });
	}

	async publish(prices: OraclePrice[], slot: number): Promise<void> {
		const timestamp = Math.floor(Date.now() / 1000);
		await Promise.all(
			prices.map((price) => this.publishOne(price, slot, timestamp))
		);
	}

	private async publishOne(
		{ symbol, oracle, price, confidence }: OraclePrice,
		slot: number,
		timestamp: number
	): Promise<void> {
		const previous = this.lastPrices.get(symbol);
		const priceChange = previous ? (price - previous) / previous : 0;
		const data = {
			oracle,
			symbol,
			price,
			confidence,
			timestamp,
			slot,
			priceChange,
		};

		await this.client.send(
			new PublishCommand({
				TopicArn: this.fifoTopicArn,
				Message: JSON.stringify({ type: 'PRICE_ALERT', data }),
				MessageAttributes: {
					type: { DataType: 'String', StringValue: 'PRICE_ALERT' },
					oracle: { DataType: 'String', StringValue: oracle },
					symbol: { DataType: 'String', StringValue: symbol },
				},
				MessageGroupId: symbol,
				MessageDeduplicationId: `${symbol}-${slot}-${timestamp}`,
			})
		);
		this.lastPrices.set(symbol, price);
	}
}
