import { expect } from 'chai';

import {
	BN,
	BASE_PRECISION,
	PRICE_PRECISION,
	QUOTE_PRECISION,
	L2Level,
	uncrossL2,
} from '../../src';

function asksAreSortedAsc(asks: L2Level[]): boolean {
	return asks.every((ask, i) => i === 0 || ask.price.gt(asks[i - 1].price));
}

function bidsAreSortedDesc(bids: L2Level[]): boolean {
	return bids.every((bid, i) => i === 0 || bid.price.lt(bids[i - 1].price));
}

describe('uncrossL2', () => {
	it('shifts a bid crossing an ask above the reference price', () => {
		const bids = [
			{
				price: new BN(104).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(103).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(102).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { clob: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(100).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const asks = [
			{
				price: new BN(101).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(102).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const oraclePrice = new BN(100).mul(QUOTE_PRECISION);
		const oracleTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const markTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const groupingSize = QUOTE_PRECISION.divn(10);

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oracleTwap5Min,
			markTwap5Min,
			groupingSize,
			new Set<string>(),
			new Set<string>()
		);

		expect(newBids[0].price.toString()).to.equal(
			new BN(101).mul(QUOTE_PRECISION).sub(groupingSize).toString()
		);
		expect(newBids[0].size.toString()).to.equal(
			new BN(3).mul(BASE_PRECISION).toString()
		);
		expect(newBids[0].sources['vamm'].toString()).to.equal(
			new BN(2).mul(BASE_PRECISION).toString()
		);
		expect(newBids[0].sources['clob'].toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);

		expect(newBids[1].price.toString()).to.equal(
			new BN(100).mul(QUOTE_PRECISION).toString()
		);
		expect(newBids[1].size.toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);

		expect(newAsks[0].price.toString()).to.equal(
			new BN(101).mul(QUOTE_PRECISION).toString()
		);
		expect(newAsks[0].size.toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);

		expect(newAsks[1].price.toString()).to.equal(
			new BN(102).mul(QUOTE_PRECISION).toString()
		);
		expect(newAsks[1].size.toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);
	});

	it('shifts an ask crossing a bid below the reference price', () => {
		const bids = [
			{
				price: new BN(99).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(98).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const asks = [
			{
				price: new BN(96).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(97).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(98).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { clob: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(100).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const oraclePrice = new BN(100).mul(QUOTE_PRECISION);
		const oracleTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const markTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const groupingSize = QUOTE_PRECISION.divn(10);

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oracleTwap5Min,
			markTwap5Min,
			groupingSize,
			new Set<string>(),
			new Set<string>()
		);

		expect(newBids[0].price.toString()).to.equal(
			new BN(99).mul(QUOTE_PRECISION).toString()
		);
		expect(newBids[1].price.toString()).to.equal(
			new BN(98).mul(QUOTE_PRECISION).toString()
		);

		expect(newAsks[0].price.toString()).to.equal(
			new BN(99).mul(QUOTE_PRECISION).add(groupingSize).toString()
		);
		expect(newAsks[0].size.toString()).to.equal(
			new BN(3).mul(BASE_PRECISION).toString()
		);
		expect(newAsks[0].sources['vamm'].toString()).to.equal(
			new BN(2).mul(BASE_PRECISION).toString()
		);
		expect(newAsks[0].sources['clob'].toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);

		expect(newAsks[1].price.toString()).to.equal(
			new BN(100).mul(QUOTE_PRECISION).toString()
		);
	});

	it('leaves an already-uncrossed book unchanged', () => {
		const bids = [
			{
				price: new BN(99).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(98).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(97).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const asks = [
			{
				price: new BN(101).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(102).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const oraclePrice = new BN(100).mul(QUOTE_PRECISION);
		const oracleTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const markTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const groupingSize = QUOTE_PRECISION.divn(10);

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oracleTwap5Min,
			markTwap5Min,
			groupingSize,
			new Set<string>(),
			new Set<string>()
		);

		expect(newBids.map((b) => b.price.toString())).to.deep.equal(
			bids.map((b) => b.price.toString())
		);
		expect(newAsks.map((a) => a.price.toString())).to.deep.equal(
			asks.map((a) => a.price.toString())
		);
	});

	it('centers a cross that straddles the reference price', () => {
		const bids = [
			{
				price: new BN(32).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { clob: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const asks = [
			{
				price: new BN(29).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const oraclePrice = new BN('29250100');
		const oracleTwap5Min = new BN('29696597');
		const markTwap5Min = new BN('31747865');
		const groupingSize = QUOTE_PRECISION.divn(10);

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oracleTwap5Min,
			markTwap5Min,
			groupingSize,
			new Set<string>(),
			new Set<string>()
		);

		const referencePrice = oraclePrice.add(markTwap5Min.sub(oracleTwap5Min));

		expect(newBids[0].price.toString()).to.equal(
			referencePrice.sub(groupingSize).toString()
		);
		expect(newAsks[0].price.toString()).to.equal(
			referencePrice.add(groupingSize).toString()
		);
	});

	it('leaves a user bid at its price and shifts the rest around it', () => {
		const bids = [
			{
				price: new BN(104).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { clob: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(103).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(102).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { clob: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(100).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const asks = [
			{
				price: new BN(101).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(102).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const oraclePrice = new BN(100).mul(QUOTE_PRECISION);
		const oracleTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const markTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const groupingSize = QUOTE_PRECISION.divn(10);

		const userBids = new Set<string>([
			new BN(104).mul(QUOTE_PRECISION).toString(),
		]);

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oracleTwap5Min,
			markTwap5Min,
			groupingSize,
			userBids,
			new Set<string>()
		);

		expect(newBids[0].price.toString()).to.equal(
			new BN(104).mul(QUOTE_PRECISION).toString()
		);
		expect(newBids[0].sources['clob'].toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);

		expect(newBids[1].price.toString()).to.equal(
			new BN(101).mul(QUOTE_PRECISION).sub(groupingSize).toString()
		);
		expect(newBids[1].size.toString()).to.equal(
			new BN(2).mul(BASE_PRECISION).toString()
		);
		expect(newBids[1].sources['vamm'].toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);
		expect(newBids[1].sources['clob'].toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);

		expect(newBids[2].price.toString()).to.equal(
			new BN(100).mul(QUOTE_PRECISION).toString()
		);

		expect(newAsks[0].price.toString()).to.equal(
			new BN(101).mul(QUOTE_PRECISION).toString()
		);
		expect(newAsks[1].price.toString()).to.equal(
			new BN(102).mul(QUOTE_PRECISION).toString()
		);
	});

	it('leaves a user ask at its price and shifts the rest around it', () => {
		const bids = [
			{
				price: new BN(99).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(98).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const asks = [
			{
				price: new BN(96).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { clob: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(97).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(98).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { clob: new BN(1).mul(BASE_PRECISION) },
			},
			{
				price: new BN(100).mul(QUOTE_PRECISION),
				size: new BN(1).mul(BASE_PRECISION),
				sources: { vamm: new BN(1).mul(BASE_PRECISION) },
			},
		];

		const oraclePrice = new BN(100).mul(QUOTE_PRECISION);
		const oracleTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const markTwap5Min = new BN(100).mul(QUOTE_PRECISION);
		const groupingSize = QUOTE_PRECISION.divn(10);

		const userAsks = new Set<string>([
			new BN(96).mul(QUOTE_PRECISION).toString(),
		]);

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oracleTwap5Min,
			markTwap5Min,
			groupingSize,
			new Set<string>(),
			userAsks
		);

		expect(newBids[0].price.toString()).to.equal(
			new BN(99).mul(QUOTE_PRECISION).toString()
		);
		expect(newBids[1].price.toString()).to.equal(
			new BN(98).mul(QUOTE_PRECISION).toString()
		);

		expect(newAsks[0].price.toString()).to.equal(
			new BN(96).mul(QUOTE_PRECISION).toString()
		);
		expect(newAsks[0].sources['clob'].toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);

		expect(newAsks[1].price.toString()).to.equal(
			new BN(99).mul(QUOTE_PRECISION).add(groupingSize).toString()
		);
		expect(newAsks[1].size.toString()).to.equal(
			new BN(2).mul(BASE_PRECISION).toString()
		);
		expect(newAsks[1].sources['vamm'].toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);
		expect(newAsks[1].sources['clob'].toString()).to.equal(
			new BN(1).mul(BASE_PRECISION).toString()
		);

		expect(newAsks[2].price.toString()).to.equal(
			new BN(100).mul(QUOTE_PRECISION).toString()
		);
	});

	it('keeps a user bid in place among many overlapping levels', () => {
		const oraclePrice = new BN(190.3843 * PRICE_PRECISION.toNumber());
		const bids = [
			[190.59, 2],
			[190.588, 58.3],
			[190.5557, 5],
			[190.5547, 5],
			[190.5508, 5],
			[190.541, 2],
			[190.5099, 49.1],
			[190.5, 60],
		].map(([price, size]) => ({
			price: new BN(price * PRICE_PRECISION.toNumber()),
			size: new BN(size * BASE_PRECISION.toNumber()),
			sources: { vamm: new BN(size * BASE_PRECISION.toNumber()) },
		}));

		const asks = [
			[190.5, 86.5],
			[190.6159, 1],
			[190.656, 10.5],
			[190.6561, 1],
			[190.6585, 5],
			[190.6595, 5],
			[190.6596, 5],
		].map(([price, size]) => ({
			price: new BN(price * PRICE_PRECISION.toNumber()),
			size: new BN(size * BASE_PRECISION.toNumber()),
			sources: { vamm: new BN(size * BASE_PRECISION.toNumber()) },
		}));

		expect(asksAreSortedAsc(asks), 'input asks are ascending').to.be.true;
		expect(bidsAreSortedDesc(bids), 'input bids are descending').to.be.true;

		const groupingSize = new BN('100');
		const userBidPrice = new BN(190.588 * PRICE_PRECISION.toNumber());
		const userBids = new Set<string>([userBidPrice.toString()]);

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oraclePrice,
			oraclePrice,
			groupingSize,
			userBids,
			new Set<string>()
		);

		expect(asksAreSortedAsc(newAsks), 'uncrossed asks stay ascending').to.be
			.true;
		expect(bidsAreSortedDesc(newBids), 'uncrossed bids stay descending').to.be
			.true;
		expect(newBids[0].price.toString()).to.equal(userBidPrice.toString());
	});

	it('keeps output sorted through a large cross with an overlapping level', () => {
		const bids = [
			'104411000',
			'103835800',
			'103826259',
			'103825000',
			'103822000',
			'103821500',
			'103820283',
			'103816900',
			'103816000',
			'103815121',
		].map((priceStr) => ({
			price: new BN(priceStr),
			size: new BN(1).mul(BASE_PRECISION),
			sources: { vamm: new BN(1).mul(BASE_PRECISION) },
		}));

		const asks = [
			'103822000',
			'103838354',
			'103843087',
			'103843351',
			'103843880',
			'103845114',
			'103846148',
			'103850100',
			'103851300',
			'103854304',
		].map((priceStr) => ({
			price: new BN(priceStr),
			size: new BN(1).mul(BASE_PRECISION),
			sources: { vamm: new BN(1).mul(BASE_PRECISION) },
		}));

		expect(asksAreSortedAsc(asks), 'input asks are ascending').to.be.true;
		expect(bidsAreSortedDesc(bids), 'input bids are descending').to.be.true;

		const oraclePrice = new BN('103649895');
		const oracleTwap5Min = new BN('103285000');
		const markTwap5Min = new BN('103371000');
		const groupingSize = new BN('100');

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oracleTwap5Min,
			markTwap5Min,
			groupingSize,
			new Set<string>(),
			new Set<string>()
		);

		expect(asksAreSortedAsc(newAsks), 'uncrossed asks stay ascending').to.be
			.true;
		expect(bidsAreSortedDesc(newBids), 'uncrossed bids stay descending').to.be
			.true;
	});

	it('keeps output sorted when only the top level crosses by a wide margin', () => {
		const bids = [
			'101825900',
			'101783900',
			'101783000',
			'101782600',
			'101770700',
			'101770200',
			'101749857',
			'101735900',
			'101729994',
			'101726900',
		].map((priceStr) => ({
			price: new BN(priceStr),
			size: new BN(1).mul(BASE_PRECISION),
			sources: { vamm: new BN(1).mul(BASE_PRECISION) },
		}));

		const asks = [
			'101750700',
			'101790467',
			'101793400',
			'101794116',
			'101798548',
			'101799532',
			'101803500',
			'101820927',
			'101823900',
			'101827638',
		].map((priceStr) => ({
			price: new BN(priceStr),
			size: new BN(1).mul(BASE_PRECISION),
			sources: { vamm: new BN(1).mul(BASE_PRECISION) },
		}));

		expect(asksAreSortedAsc(asks), 'input asks are ascending').to.be.true;
		expect(bidsAreSortedDesc(bids), 'input bids are descending').to.be.true;

		const oraclePrice = new BN('101711384');
		const oracleTwap5Min = new BN('101805000');
		const markTwap5Min = new BN('101867000');
		const groupingSize = new BN('100');

		const { bids: newBids, asks: newAsks } = uncrossL2(
			bids,
			asks,
			oraclePrice,
			oracleTwap5Min,
			markTwap5Min,
			groupingSize,
			new Set<string>(),
			new Set<string>()
		);

		expect(asksAreSortedAsc(newAsks), 'uncrossed asks stay ascending').to.be
			.true;
		expect(bidsAreSortedDesc(newBids), 'uncrossed bids stay descending').to.be
			.true;
	});
});
