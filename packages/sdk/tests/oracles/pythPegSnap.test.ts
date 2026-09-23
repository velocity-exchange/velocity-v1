import * as pythClientLib from '@pythnetwork/client';
import * as sinon from 'sinon';
import { PythClient } from '../../src/oracles/pythClient';
import { PythLazerClient } from '../../src/oracles/pythLazerClient';
import { BN, QUOTE_PRECISION } from '../../src';
import { assert } from '../../src/assert/assert';

// Program's `get_pyth_stable_coin_price` snaps to peg when
// `|price - QUOTE_PRECISION| <= min(confidence, five_bps)` (note `<=`, not `<`).
describe('Pyth stablecoin peg-snap boundary', () => {
	afterEach(() => {
		sinon.restore();
	});

	describe('PythClient', () => {
		// `PythClient` reads the pyth v2 price account header before it decodes,
		// so a stubbed decode still needs a buffer that carries one.
		function pythPriceAccountBuffer(): Buffer {
			const buffer = Buffer.alloc(3312);
			buffer.writeUInt32LE(0xa1b2c3d4, 0);
			buffer.writeUInt32LE(2, 4);
			buffer.writeUInt32LE(3, 8);
			return buffer;
		}

		function stubParsePriceData(priceAboveQuote: number, confidence: number) {
			sinon.stub(pythClientLib, 'parsePriceData').returns({
				exponent: -6,
				aggregate: { price: 1 + priceAboveQuote / 1_000_000 },
				confidence: confidence / 1_000_000,
				twap: { value: 1 },
				twac: { value: 0 },
				lastSlot: { toString: () => '1' },
				numComponentPrices: 3,
				numQuoters: 3,
			} as any);
		}

		it('snaps to peg exactly at the confidence bound', () => {
			// spread == min(confidence, fiveBPS) == 500 exactly
			stubParsePriceData(500, 1000);
			const client = new PythClient({} as any, undefined, true);
			const data = client.getOraclePriceDataFromBuffer(
				pythPriceAccountBuffer()
			);
			assert(data.price.eq(QUOTE_PRECISION));
		});

		it('does not snap just past the confidence bound', () => {
			stubParsePriceData(501, 1000);
			const client = new PythClient({} as any, undefined, true);
			const data = client.getOraclePriceDataFromBuffer(
				pythPriceAccountBuffer()
			);
			assert(!data.price.eq(QUOTE_PRECISION));
			assert(data.price.eq(QUOTE_PRECISION.add(new BN(501))));
		});
	});

	describe('PythClient header check', () => {
		it('rejects an account that is not a pyth price account', () => {
			const client = new PythClient({} as any, undefined, true);
			const buffer = Buffer.alloc(3312);
			buffer.writeUInt32LE(0xa1b2c3d4, 0);
			buffer.writeUInt32LE(2, 4);
			buffer.writeUInt32LE(2, 8); // AccountType::Product
			let threw = false;
			try {
				client.getOraclePriceDataFromBuffer(buffer);
			} catch (_e) {
				threw = true;
			}
			assert(threw);
		});

		it('rejects an account without the pyth magic', () => {
			const client = new PythClient({} as any, undefined, true);
			let threw = false;
			try {
				client.getOraclePriceDataFromBuffer(Buffer.alloc(3312));
			} catch (_e) {
				threw = true;
			}
			assert(threw);
		});

		it('rejects an account shorter than a price account', () => {
			const client = new PythClient({} as any, undefined, true);
			const buffer = Buffer.alloc(12);
			buffer.writeUInt32LE(0xa1b2c3d4, 0);
			buffer.writeUInt32LE(2, 4);
			buffer.writeUInt32LE(3, 8);
			let threw = false;
			try {
				client.getOraclePriceDataFromBuffer(buffer);
			} catch (_e) {
				threw = true;
			}
			assert(threw);
		});
	});

	describe('PythLazerClient', () => {
		function makeClient(priceAboveQuote: number, confidence: number) {
			const client = new PythLazerClient(
				{ commitment: 'confirmed' } as any,
				undefined,
				true
			);
			(client as any).decodeFunc = () => ({
				price: QUOTE_PRECISION.add(new BN(priceAboveQuote)),
				conf: new BN(confidence),
				exponent: -6,
				postedSlot: new BN(1),
				publishTime: new BN(1),
			});
			return client;
		}

		it('snaps to peg exactly at the confidence bound', () => {
			const client = makeClient(500, 1000);
			const data = client.getOraclePriceDataFromBuffer(Buffer.alloc(1));
			assert(data.price.eq(QUOTE_PRECISION));
		});

		it('does not snap just past the confidence bound', () => {
			const client = makeClient(501, 1000);
			const data = client.getOraclePriceDataFromBuffer(Buffer.alloc(1));
			assert(!data.price.eq(QUOTE_PRECISION));
			assert(data.price.eq(QUOTE_PRECISION.add(new BN(501))));
		});
	});
});
