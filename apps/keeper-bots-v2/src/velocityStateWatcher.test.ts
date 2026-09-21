import { expect } from 'chai';
import { VelocityStateWatcher } from './velocityStateWatcher';

// Minimal stand-in for VelocityClient: only what the watcher reads.
function stubClient(numberOfSpotMarkets: number) {
	return {
		isSubscribed: true,
		getStateAccount: () => ({ numberOfMarkets: 1, numberOfSpotMarkets }),
		getPerpMarketAccounts: () => [],
		getSpotMarketAccounts: () => [],
	};
}

describe('VelocityStateWatcher', () => {
	it('notifies once per change, not on every tick', () => {
		let spotMarkets = 4;
		const messages: string[] = [];
		const client = stubClient(spotMarkets);
		client.getStateAccount = () => ({
			numberOfMarkets: 1,
			numberOfSpotMarkets: spotMarkets,
		});

		const watcher = new VelocityStateWatcher({
			velocityClient: client as any,
			intervalMs: 10_000,
			stateChecks: {
				newPerpMarkets: true,
				newSpotMarkets: true,
				perpMarketStatus: false,
				spotMarketStatus: false,
				onStateChange: (message: string) => {
					messages.push(message);
				},
			},
		});
		watcher.subscribe();
		watcher.unsubscribe();

		const tick = () => (watcher as any).checkForUpdates();

		tick();
		expect(messages).to.have.lengthOf(0);

		spotMarkets = 5;
		tick();
		tick();
		tick();

		expect(messages).to.have.lengthOf(1);
		expect(messages[0]).to.contain('4 -> 5');
		// health stays failed so the pod still restarts
		expect(watcher.triggered).to.equal(true);
	});
});
