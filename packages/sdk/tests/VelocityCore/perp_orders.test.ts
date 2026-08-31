import { describe, expect, test } from 'bun:test';
import { Keypair } from '@solana/web3.js';
import { VelocityCore } from '../../src/core/VelocityCore';

describe('VelocityCore perp order instruction builders', () => {
	const pk = () => Keypair.generate().publicKey;
	const fakeIx = {
		keys: [],
		programId: Keypair.generate().publicKey,
		data: Buffer.alloc(0),
	};

	test('buildPlacePerpOrderInstruction', async () => {
		const called: any[] = [];
		const program = {
			instruction: {
				placePerpOrder: async (...args: any[]) => {
					called.push(args);
					return fakeIx;
				},
			},
		};
		const ix = await VelocityCore.buildPlacePerpOrderInstruction({
			program,
			orderParams: { x: 1 },
			state: pk(),
			user: pk(),
			userStats: pk(),
			authority: pk(),
			remainingAccounts: [],
		});
		expect(ix).toBe(fakeIx as any);
		expect(called[0][0]).toEqual({ x: 1 });
	});

	test('buildPlaceAndTakePerpOrderInstruction', async () => {
		const called: any[] = [];
		const programId = pk();
		const program = {
			programId,
			instruction: {
				placeAndTakePerpOrder: async (...args: any[]) => {
					called.push(['v0', ...args]);
					return fakeIx;
				},
				placeAndTakePerpOrderV1: async (...args: any[]) => {
					called.push(['v1', ...args]);
					return fakeIx;
				},
			},
		};
		// Omitted CLOB accounts select the v0 route, whose account list carries
		// no CLOB accounts at all.
		const ix = await VelocityCore.buildPlaceAndTakePerpOrderInstruction({
			program,
			orderParams: { m: 0 },
			optionalParams: 256,
			state: pk(),
			user: pk(),
			userStats: pk(),
			authority: pk(),
			remainingAccounts: [],
		});
		expect(ix).toBe(fakeIx as any);
		expect(called[0][0]).toBe('v0');
		expect(called[0][2]).toBe(256);

		// CLOB accounts select the v1 route; an omitted crankConditions encodes
		// as the program id (anchor's `None`).
		const clobAccounts = {
			quoter: pk(),
			clobMarket: pk(),
			clobProgram: pk(),
			clobAuthority: pk(),
		};
		await VelocityCore.buildPlaceAndTakePerpOrderInstruction({
			program,
			orderParams: { m: 0 },
			optionalParams: null,
			state: pk(),
			user: pk(),
			userStats: pk(),
			authority: pk(),
			remainingAccounts: [],
			clobAccounts,
		});
		expect(called[1][0]).toBe('v1');
		const withClob = called[1][3].accounts;
		expect(withClob.quoter).toBe(clobAccounts.quoter);
		expect(withClob.clobMarket).toBe(clobAccounts.clobMarket);
		expect(withClob.clobProgram).toBe(clobAccounts.clobProgram);
		expect(withClob.clobAuthority).toBe(clobAccounts.clobAuthority);
		expect(withClob.crankConditions).toBe(programId);
	});

	test('buildPlaceAndMakePerpOrderInstruction', async () => {
		const called: any[] = [];
		const programId = pk();
		const program = {
			programId,
			instruction: {
				placeAndMakePerpOrderV1: async (...args: any[]) => {
					called.push(args);
					return fakeIx;
				},
			},
		};
		const clobAccounts = {
			quoter: pk(),
			clobMarket: pk(),
			clobProgram: pk(),
			clobAuthority: pk(),
		};
		const ix = await VelocityCore.buildPlaceAndMakePerpOrderInstruction({
			program,
			orderParams: {},
			state: pk(),
			user: pk(),
			userStats: pk(),
			authority: pk(),
			remainingAccounts: [],
			clobAccounts,
		});
		expect(ix).toBe(fakeIx as any);
		// v1 takes only orderParams, then the accounts object — no taker.
		expect(called[0][1].accounts.quoter).toBe(clobAccounts.quoter);
		// An omitted crankConditions encodes as the program id.
		expect(called[0][1].accounts.crankConditions).toBe(programId);
	});

	test('buildCancelOrderInstruction', async () => {
		const called: any[] = [];
		const program = {
			instruction: {
				cancelOrder: async (...args: any[]) => {
					called.push(args);
					return fakeIx;
				},
			},
		};
		const ix = await VelocityCore.buildCancelOrderInstruction({
			program,
			orderId: 3,
			state: pk(),
			user: pk(),
			authority: pk(),
			remainingAccounts: [],
		});
		expect(ix).toBe(fakeIx as any);
		expect(called[0][0]).toBe(3);
	});

	test('buildCancelOrderByUserIdInstruction', async () => {
		const called: any[] = [];
		const program = {
			instruction: {
				cancelOrderByUserId: async (...args: any[]) => {
					called.push(args);
					return fakeIx;
				},
			},
		};
		const ix = await VelocityCore.buildCancelOrderByUserIdInstruction({
			program,
			userOrderId: 9,
			state: pk(),
			user: pk(),
			authority: pk(),
			oracle: pk(),
			remainingAccounts: [],
		});
		expect(ix).toBe(fakeIx as any);
		expect(called[0][0]).toBe(9);
	});

	test('buildCancelOrdersByIdsInstruction', async () => {
		const called: any[] = [];
		const program = {
			instruction: {
				cancelOrdersByIds: async (...args: any[]) => {
					called.push(args);
					return fakeIx;
				},
			},
		};
		const ids = [1, 2];
		const ix = await VelocityCore.buildCancelOrdersByIdsInstruction({
			program,
			orderIds: ids,
			state: pk(),
			user: pk(),
			authority: pk(),
			remainingAccounts: [],
		});
		expect(ix).toBe(fakeIx as any);
		expect(called[0][0]).toBe(ids);
	});

	test('buildModifyOrderInstruction', async () => {
		const called: any[] = [];
		const program = {
			instruction: {
				modifyOrder: async (...args: any[]) => {
					called.push(args);
					return fakeIx;
				},
			},
		};
		const mp = { baseAssetAmount: null };
		const ix = await VelocityCore.buildModifyOrderInstruction({
			program,
			orderId: 4,
			modifyParams: mp,
			state: pk(),
			user: pk(),
			userStats: pk(),
			authority: pk(),
			remainingAccounts: [],
		});
		expect(ix).toBe(fakeIx as any);
		expect(called[0][0]).toBe(4);
		expect(called[0][1]).toBe(mp);
	});

	test('buildModifyOrderByUserIdInstruction', async () => {
		const called: any[] = [];
		const program = {
			instruction: {
				modifyOrderByUserId: async (...args: any[]) => {
					called.push(args);
					return fakeIx;
				},
			},
		};
		const mp = { reduceOnly: false };
		const ix = await VelocityCore.buildModifyOrderByUserIdInstruction({
			program,
			userOrderId: 11,
			modifyParams: mp,
			state: pk(),
			user: pk(),
			userStats: pk(),
			authority: pk(),
			remainingAccounts: [],
		});
		expect(ix).toBe(fakeIx as any);
		expect(called[0][0]).toBe(11);
		expect(called[0][1]).toBe(mp);
	});
});
