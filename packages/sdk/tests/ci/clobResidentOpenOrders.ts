/**
 * Parity test: `clobResidentOpenOrders` must match `User::clob_resident_open_orders`
 * in programs/velocity/src/state/user.rs.
 */
import { assert } from 'chai';
import {
	BN,
	clobResidentOpenOrders,
	MarketType,
	OrderBitFlag,
	OrderStatus,
	UserAccount,
} from '../../src';

const MARKET = 3;

function user(openOrders: number, rows: { flags: number }[]): UserAccount {
	return {
		perpPositions: [
			{
				marketIndex: MARKET,
				openOrders,
				baseAssetAmount: new BN(0),
				quoteAssetAmount: new BN(0),
				isolatedPositionScaledBalance: new BN(0),
				positionFlag: 0,
			},
		],
		orders: rows.map((row) => ({
			status: OrderStatus.OPEN,
			marketType: MarketType.PERP,
			marketIndex: MARKET,
			bitFlags: row.flags,
		})),
	} as unknown as UserAccount;
}

describe('clobResidentOpenOrders', () => {
	it('counts the open orders that no slot row lists', () => {
		assert.equal(clobResidentOpenOrders(user(3, [{ flags: 0 }]), MARKET), 2);
	});

	it('counts a placed-trigger shadow as book-resident', () => {
		assert.equal(
			clobResidentOpenOrders(
				user(1, [{ flags: OrderBitFlag.PlacedOnClob }]),
				MARKET
			),
			1
		);
	});

	it('answers zero for a market the account holds nothing in', () => {
		assert.equal(clobResidentOpenOrders(user(2, []), MARKET + 1), 0);
	});
});
