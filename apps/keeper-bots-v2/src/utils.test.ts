import { expect } from 'chai';
import { isSetComputeUnitsIx, logWideEvent } from './utils';
import { ComputeBudgetProgram } from '@solana/web3.js';
import { logger } from './logger';

describe('transaction simulation tests', () => {
	it('isSetComputeUnitsIx', () => {
		const cuLimitIx = ComputeBudgetProgram.setComputeUnitLimit({
			units: 1_400_000,
		});
		const cuPriceIx = ComputeBudgetProgram.setComputeUnitPrice({
			microLamports: 10_000,
		});

		expect(isSetComputeUnitsIx(cuLimitIx)).to.be.true;
		expect(isSetComputeUnitsIx(cuPriceIx)).to.be.false;
	});
});

describe('logWideEvent', () => {
	const captured: string[] = [];
	let originalInfo: typeof logger.info;

	beforeEach(() => {
		captured.length = 0;
		originalInfo = logger.info.bind(logger);
		// eslint-disable-next-line @typescript-eslint/no-explicit-any
		(logger as any).info = (message: string) => {
			captured.push(message);
			return logger;
		};
	});

	afterEach(() => {
		// eslint-disable-next-line @typescript-eslint/no-explicit-any
		(logger as any).info = originalInfo;
	});

	it('emits one line that is exactly one JSON object', () => {
		logWideEvent('tx', { status: 'ok', market: 0 });
		expect(captured).to.have.length(1);
		const line = captured[0];
		expect(line.includes('\n')).to.be.false;
		expect(line.indexOf('{')).to.equal(0);
		expect(line.lastIndexOf('}')).to.equal(line.length - 1);
		// The dashboard extracts from the first `{` to the last `}` and parses it.
		expect(JSON.parse(line)).to.deep.equal({
			event: 'tx',
			market: 0,
			status: 'ok',
		});
	});

	it('carries the line filter the dashboard matches on', () => {
		logWideEvent('fill_decision', { action: 'sent' });
		expect(captured[0].includes('"event":"fill_decision"')).to.be.true;
	});

	it('serializes keys alphabetically, like serde_json on the rust filler', () => {
		logWideEvent('tx', { status: 'ok', actual_fills: 1, market: 0 });
		expect(captured[0]).to.equal(
			'{"actual_fills":1,"event":"tx","market":0,"status":"ok"}'
		);
	});

	it('drops undefined fields so an unknown dimension is an absent column', () => {
		logWideEvent('tx', { order_id: undefined, synthetic_order_id: 42 });
		expect(JSON.parse(captured[0])).to.deep.equal({
			event: 'tx',
			synthetic_order_id: 42,
		});
	});

	it('keeps null through, so an explicitly-known-empty value is distinguishable', () => {
		logWideEvent('tx', { error: null });
		expect(JSON.parse(captured[0])).to.deep.equal({ event: 'tx', error: null });
	});
});
