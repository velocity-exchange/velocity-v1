import { expect } from 'chai';
import sinon from 'sinon';
import {
	AddressLookupTableAccount,
	Connection,
	PublicKey,
} from '@solana/web3.js';
import { TitanClient } from '../../src/titan/titanClient';

const ALT_KEY = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');

/** Resolves to the rejection reason, or fails if the promise resolves. */
const captureError = async (promise: Promise<unknown>): Promise<Error> => {
	try {
		await promise;
	} catch (err) {
		return err as Error;
	}

	throw new Error('expected the promise to reject, but it resolved');
};

describe('TitanClient.fetchLookupTable', () => {
	let connection: sinon.SinonStubbedInstance<Connection>;
	let client: TitanClient;

	const lookupTable = { key: ALT_KEY } as AddressLookupTableAccount;

	// Private — a route's lookup tables all have to resolve or the transaction
	// silently exceeds the size limit, so the retry behaviour is worth pinning.
	const fetchLookupTable = (): Promise<AddressLookupTableAccount> =>
		(
			client as unknown as {
				fetchLookupTable: (k: PublicKey) => Promise<AddressLookupTableAccount>;
			}
		).fetchLookupTable(ALT_KEY);

	beforeEach(() => {
		connection = sinon.createStubInstance(Connection);
		client = new TitanClient({
			connection: connection as unknown as Connection,
			authToken: '',
		});
	});

	afterEach(() => {
		sinon.restore();
	});

	it('retries a transient RPC failure and returns the table on success', async () => {
		connection.getAddressLookupTable
			.onFirstCall()
			.rejects(new Error('429 Too Many Requests'))
			.onSecondCall()
			.resolves({ context: { slot: 1 }, value: lookupTable });

		expect(await fetchLookupTable()).to.equal(lookupTable);
		expect(connection.getAddressLookupTable.callCount).to.equal(2);
	});

	it('throws with the final error once retries are exhausted', async () => {
		connection.getAddressLookupTable.rejects(new Error('429 Too Many Requests'));

		const err = await captureError(fetchLookupTable());

		expect(err.message).to.contain('Failed to fetch address lookup table');
		expect(err.message).to.contain(ALT_KEY.toString());
		expect(err.message).to.contain('429 Too Many Requests');
		// initial attempt + LOOKUP_TABLE_FETCH_RETRIES
		expect(connection.getAddressLookupTable.callCount).to.equal(3);
	});

	it('fails fast when the table does not exist on-chain', async () => {
		connection.getAddressLookupTable.resolves({
			context: { slot: 1 },
			value: null,
		});

		const err = await captureError(fetchLookupTable());

		expect(err.message).to.contain('does not exist');
		// A missing table won't appear on a retry, so don't spend attempts on it.
		expect(connection.getAddressLookupTable.calledOnce).to.be.true;
	});

	it('does not silently drop an unresolvable table', async () => {
		// Regression guard: this used to be caught, warned, and skipped, which
		// built a route without the table and blew the transaction size limit.
		connection.getAddressLookupTable.rejects(new Error('Failed to fetch'));

		const err = await captureError(fetchLookupTable());

		expect(err).to.be.instanceOf(Error);
	});
});
