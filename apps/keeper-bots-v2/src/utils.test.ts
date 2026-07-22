import { expect } from 'chai';
import {
	fillCorrelationSuffix,
	getFillTakerRefs,
	isSetComputeUnitsIx,
} from './utils';
import { NodeToFill } from '@velocity-exchange/sdk';
import { ComputeBudgetProgram } from '@solana/web3.js';

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
