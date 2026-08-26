import { expect } from 'chai';
import { readFileSync } from 'fs';
import path from 'path';
import { PositionDirection } from '../../src/types';
import { deserializeUserClobOrder } from '../../src/clob/userOrdersClient';

/**
 * The feed's wire form is decimal strings in on-chain precision, matching the
 * book documents. What matters on the way in is that the two id spaces stay
 * distinct: `orderId` is velocity's — what a client names the order by
 * everywhere else — and `nodeIndex`/`clobOrderId` are the book's handle, which
 * together are the hint a cancel takes.
 */
describe('user CLOB orders feed', () => {
	// The producer's own test asserts it emits exactly this file. A literal
	// written here instead would be blind to the publisher dropping a field:
	// both suites would pass and the feed would be broken.
	const row = JSON.parse(
		readFileSync(
			path.join(__dirname, '../fixtures/userClobOrderRow.json'),
			'utf8'
		)
	);

	it('names its own market, so a flat cross-market list is usable', () => {
		expect(deserializeUserClobOrder(row).marketIndex).to.equal(3);
	});

	it('keeps velocity ids and book handles apart', () => {
		const order = deserializeUserClobOrder(row);
		expect(order.orderId).to.equal(41);
		expect(order.nodeIndex).to.equal(7);
		// A book order id is a u64 and outgrows a JS number, so it stays a BN.
		expect(order.clobOrderId.toString()).to.equal('18446744073709551615');
	});

	it('reads sides and precision-bearing fields as the chain states them', () => {
		const order = deserializeUserClobOrder(row);
		expect(order.direction).to.deep.equal(PositionDirection.SHORT);
		expect(order.price.toString()).to.equal('99000000');
		expect(order.baseAssetAmount.toString()).to.equal('500000000');
		expect(
			deserializeUserClobOrder({ ...row, direction: 'long' }).direction
		).to.deep.equal(PositionDirection.LONG);
	});

	it('reports a migrated taker remainder as one', () => {
		expect(deserializeUserClobOrder(row).takerOrigin).to.equal(false);
		expect(
			deserializeUserClobOrder({ ...row, takerOrigin: true }).takerOrigin
		).to.equal(true);
	});
});
