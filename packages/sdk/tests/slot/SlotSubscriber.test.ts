import { expect } from 'chai';
import sinon from 'sinon';
import { Connection } from '@solana/web3.js';
import { SlotSubscriber } from '../../src/slot/SlotSubscriber';

// The resub watchdog is a self-re-arming timer chain: the next timeout is only
// armed by a successful subscribe(). Anything that stops the callback reaching
// that point kills the chain permanently, with no throw and no log, and the
// subscriber then reports a frozen slot forever.
describe('SlotSubscriber resub watchdog', () => {
	let clock: sinon.SinonFakeTimers;
	let connection: sinon.SinonStubbedInstance<Connection>;
	let subscribeCount: number;

	const newSubscriber = () =>
		new SlotSubscriber(connection as unknown as Connection, {
			resubTimeoutMs: 1000,
		});

	beforeEach(() => {
		clock = sinon.useFakeTimers();
		connection = sinon.createStubInstance(Connection);
		connection.getSlot.resolves(100);
		subscribeCount = 0;
		connection.onSlotChange.callsFake(() => ++subscribeCount);
	});

	afterEach(() => {
		clock.restore();
		sinon.restore();
	});

	it('keeps resubscribing when the teardown never resolves', async () => {
		// Half-open websocket: removeSlotChangeListener never settles.
		connection.removeSlotChangeListener.returns(new Promise<void>(() => {}));

		const sub = newSubscriber();
		await sub.subscribe();
		expect(subscribeCount).to.equal(1);

		// Watchdog fires, teardown hangs, is abandoned after the unsubscribe timeout.
		await clock.tickAsync(1000);
		await clock.tickAsync(10_000);
		expect(subscribeCount).to.equal(2);

		// The chain is still armed. It wedged here before the fix.
		await clock.tickAsync(1000);
		await clock.tickAsync(10_000);
		expect(subscribeCount).to.equal(3);
	});

	it('re-arms when the resubscribe itself throws', async () => {
		connection.removeSlotChangeListener.resolves();
		connection.getSlot.onSecondCall().rejects(new Error('rpc down'));

		const sub = newSubscriber();
		await sub.subscribe();
		expect(subscribeCount).to.equal(1);

		// subscribe() throws inside the callback, so nothing armed the next timeout.
		await clock.tickAsync(1000);
		expect(subscribeCount).to.equal(1);

		// The next attempt still happens and succeeds.
		await clock.tickAsync(1000);
		expect(subscribeCount).to.equal(2);
	});

	it('stops for good on a caller-initiated unsubscribe', async () => {
		connection.removeSlotChangeListener.resolves();

		const sub = newSubscriber();
		await sub.subscribe();
		await sub.unsubscribe();

		await clock.tickAsync(60_000);
		expect(subscribeCount).to.equal(1);
	});
});
