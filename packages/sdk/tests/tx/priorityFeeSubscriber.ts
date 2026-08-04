import { expect } from 'chai';
import { Connection } from '@solana/web3.js';
import { PriorityFeeSubscriber } from '../../src/priorityFee/priorityFeeSubscriber';

describe('PriorityFeeSubscriber', () => {
	it('uses config.fetchSolanaPriorityFee instead of the default RPC call, when provided', async () => {
		const connection = new Connection('http://localhost:8899');
		let callCount = 0;
		const subscriber = new PriorityFeeSubscriber({
			connection,
			fetchSolanaPriorityFee: async () => {
				callCount++;
				return [{ slot: 1, prioritizationFee: 500 }];
			},
		});

		await subscriber.load();

		expect(callCount).to.equal(1);
		expect(subscriber.getAvgStrategyResult()).to.equal(500);
	});

	it('falls back to the default fetchSolanaPriorityFee when none is provided', () => {
		const connection = new Connection('http://localhost:8899');
		const subscriber = new PriorityFeeSubscriber({ connection });

		expect(subscriber.fetchSolanaPriorityFee).to.be.a('function');
	});
});
