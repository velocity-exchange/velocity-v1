import { expect } from 'chai';
import { VelocityStateWatcher } from './velocityStateWatcher';

describe('VelocityStateWatcher', () => {
	it('notifies once per change, not on every tick', () => {
		let spotMarkets = 4;
		const messages: string[] = [];
		// Minimal stand-in for VelocityClient: only what the watcher reads.
		const client = {
			isSubscribed: true,
			getStateAccount: () => ({
				numberOfMarkets: 1,
				numberOfSpotMarkets: spotMarkets,
			}),
			getPerpMarketAccounts: () => [],
			getSpotMarketAccounts: () => [],
		};

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

		// a further change is a different message, so it still gets through
		spotMarkets = 6;
		tick();
		expect(messages).to.have.lengthOf(2);
		expect(messages[1]).to.contain('4 -> 6');
	});
});
