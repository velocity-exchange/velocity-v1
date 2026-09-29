// bun test deploy-scripts/migrate.test.ts
import { test } from 'node:test';
import assert from 'node:assert/strict';
import bs58 from 'bs58';
import { BN } from '@coral-xyz/anchor';
import { PublicKey } from '@solana/web3.js';
import {
	getQuoterSlabPublicKey,
	transactionCost,
} from '@velocity-exchange/sdk';
import {
	bookConfig,
	BookBringUp,
	CLOB_CONFIG_FIELDS,
	DEFAULT_FEE_RAILS,
	findUnnamedBook,
	liquidationReimbursementUpdate,
	parseFeeRails,
} from './migrate';

const velocity = PublicKey.unique();
const clobProgram = PublicKey.unique();
const quoter = PublicKey.unique();
const marketIndex = 0;
const market = {
	orderStepSize: new BN(100),
	orderTickSize: new BN(10),
	marketStats: { minOrderSize: new BN(1_000) },
};
const quoterSlab = getQuoterSlabPublicKey(velocity, marketIndex);

/** An empty book whose slab authorities and config match what the migration writes. */
function emptyBook(capacity: number): Buffer {
	const data = Buffer.alloc(9648 + capacity * 104);
	quoterSlab.toBuffer().copy(data, 8);
	quoterSlab.toBuffer().copy(data, 40);
	const config = bookConfig(marketIndex, market, 1024);
	let cursor = 0;
	for (const [offset, width] of CLOB_CONFIG_FIELDS) {
		config.copy(data, offset, cursor, cursor + width);
		cursor += width;
	}

	return data;
}

type Filter =
	| { dataSize: number }
	| { memcmp: { offset: number; bytes: string } };

/** A connection that applies `dataSize` and `memcmp` filters the way an RPC node does. */
function connectionHolding(accounts: Map<PublicKey, Buffer>) {
	return {
		async getProgramAccounts(
			_program: PublicKey,
			config: {
				filters: Filter[];
				dataSlice: { offset: number; length: number };
			}
		) {
			return [...accounts]
				.filter(([, data]) =>
					config.filters.every((filter) =>
						'dataSize' in filter
							? data.length === filter.dataSize
							: data
									.subarray(filter.memcmp.offset)
									.subarray(0, 32)
									.equals(Buffer.from(bs58.decode(filter.memcmp.bytes)))
					)
				)
				.map(([pubkey, data]) => ({
					pubkey,
					account: {
						data: data.subarray(
							config.dataSlice.offset,
							config.dataSlice.offset + config.dataSlice.length
						),
					},
				}));
		},
	};
}

function bringUp(
	accounts: Map<PublicKey, Buffer>,
	pendingAccounts?: PublicKey[]
): BookBringUp {
	return {
		connection: connectionHolding(accounts),
		program: { programId: velocity },
		clobProgram,
		args: { bookCapacity: 1024 },
		admin: { pendingAccounts: () => pendingAccounts },
	} as unknown as BookBringUp;
}

test('an empty book of the expected capacity is reused', async () => {
	const book = PublicKey.unique();
	const found = await findUnnamedBook(
		bringUp(new Map([[book, emptyBook(1024)]])),
		marketIndex,
		market,
		quoter
	);
	assert.ok(found?.equals(book));
});

test('an empty book with the same config but a smaller arena is not reused', async () => {
	const found = await findUnnamedBook(
		bringUp(new Map([[PublicKey.unique(), emptyBook(514)]])),
		marketIndex,
		market,
		quoter
	);
	assert.equal(found, undefined);
});

test('the book a pending registration names wins over a lower address', async () => {
	const low = new PublicKey(Buffer.alloc(32, 1));
	const named = new PublicKey(Buffer.alloc(32, 9));
	const found = await findUnnamedBook(
		bringUp(
			new Map([
				[low, emptyBook(1024)],
				[named, emptyBook(1024)],
			]),
			[named]
		),
		marketIndex,
		market,
		quoter
	);
	assert.ok(found?.equals(named));
});

/** What relay's turner requires to land a crank: the base fee plus its priority fee. */
function turnerMinimum(priceMicroLamportsPerCu: number, units: number): number {
	return 5_000 + Math.ceil((priceMicroLamportsPerCu * units) / 1_000_000);
}

test('the default rails repay a turner bidding up to the priority ceiling', () => {
	const rails = parseFeeRails(DEFAULT_FEE_RAILS);
	assert.ok(rails.maxPriorityMicroLamportsPerCu > 0);
	for (const units of [20_000, 250_000]) {
		assert.ok(
			transactionCost(rails, units, 1) >=
				turnerMinimum(rails.maxPriorityMicroLamportsPerCu, units)
		);
	}

	assert.equal(transactionCost(rails, 250_000, 1), 7_500);
});

test('an upgrade names the SOL spot market and keeps a share already set', () => {
	const spotMarkets = new Map([
		[0, { mint: PublicKey.unique() }],
		[1, { mint: new PublicKey('So11111111111111111111111111111111111111112') }],
	]);
	assert.deepEqual(
		liquidationReimbursementUpdate(
			{ liquidationCrankReimbursementBps: 0, solSpotMarketIndex: 0 },
			spotMarkets,
			500
		),
		{ shareBps: 500, solSpotMarketIndex: 1 }
	);
	assert.deepEqual(
		liquidationReimbursementUpdate(
			{ liquidationCrankReimbursementBps: 300, solSpotMarketIndex: 0 },
			spotMarkets,
			500
		),
		{ shareBps: 300, solSpotMarketIndex: 1 }
	);
	assert.equal(
		liquidationReimbursementUpdate(
			{ liquidationCrankReimbursementBps: 300, solSpotMarketIndex: 1 },
			spotMarkets,
			500
		),
		undefined
	);
});
