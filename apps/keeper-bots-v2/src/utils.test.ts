import { expect } from 'chai';
import {
	fillCorrelationSuffix,
	getFillTakerRefs,
	isSetComputeUnitsIx,
	logWideEvent,
} from './utils';
import { NodeToFill } from '@velocity-exchange/sdk';
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

const makeNode = (taker: string, orderId: number): NodeToFill =>
	({
		node: { userAccount: taker, order: { orderId } },
		makerNodes: [],
	}) as unknown as NodeToFill;

describe('fill log correlation helpers', () => {
	it('getFillTakerRefs extracts (taker, takerOrderId) for a single-taker fill', () => {
		const refs = getFillTakerRefs([makeNode('takerA', 18)]);
		expect(refs).to.deep.equal([{ taker: 'takerA', takerOrderId: 18 }]);
	});

	it('getFillTakerRefs returns every pair for a bundled multi-taker fill', () => {
		const refs = getFillTakerRefs([
			makeNode('takerA', 18),
			makeNode('takerB', 4),
		]);
		expect(refs).to.deep.equal([
			{ taker: 'takerA', takerOrderId: 18 },
			{ taker: 'takerB', takerOrderId: 4 },
		]);
	});

	it('getFillTakerRefs skips nodes missing a userAccount or order', () => {
		const refs = getFillTakerRefs([
			makeNode('takerA', 18),
			{
				node: { order: { orderId: 9 } },
				makerNodes: [],
			} as unknown as NodeToFill,
			{
				node: { userAccount: 'takerC' },
				makerNodes: [],
			} as unknown as NodeToFill,
		]);
		expect(refs).to.deep.equal([{ taker: 'takerA', takerOrderId: 18 }]);
	});

	it('fillCorrelationSuffix renders a greppable suffix containing each taker', () => {
		const suffix = fillCorrelationSuffix([
			makeNode('takerA', 18),
			makeNode('takerB', 4),
		]);
		expect(suffix).to.equal(
			' takers: [{"taker":"takerA","takerOrderId":18},{"taker":"takerB","takerOrderId":4}]'
		);
		// A Loki line filter on either taker matches the enriched line.
		expect(suffix.includes('takerA')).to.be.true;
		expect(suffix.includes('takerB')).to.be.true;
	});

	it('fillCorrelationSuffix is empty when there are no taker refs (e.g. settlePnl)', () => {
		expect(fillCorrelationSuffix([])).to.equal('');
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
