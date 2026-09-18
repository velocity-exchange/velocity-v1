import { describe, expect, test } from 'bun:test';
import { Keypair } from '@solana/web3.js';
import { VelocityCore } from '../../src/core/VelocityCore';

describe('VelocityCore trigger builders', () => {
	const pk = () => Keypair.generate().publicKey;

	test('buildTriggerMarketOrderV1Instruction wires accounts + args', async () => {
		const called: any[] = [];
		const fakeIx = {
			keys: [],
			programId: Keypair.generate().publicKey,
			data: Buffer.alloc(0),
		};
		const programId = pk();
		const program = {
			programId,
			instruction: {
				triggerMarketOrderV1: async (...args: any[]) => {
					called.push(args);
					return fakeIx;
				},
			},
		};

		const clobMarket = pk();
		const ix = await VelocityCore.buildTriggerMarketOrderV1Instruction({
			program,
			marketIndex: 3,
			orderId: 9,
			state: pk(),
			filler: pk(),
			fillerStats: pk(),
			user: pk(),
			userStats: pk(),
			authority: pk(),
			quoterSlab: pk(),
			clobMarket,
			clobProgram: pk(),
			remainingAccounts: [],
		});

		expect(ix).toBe(fakeIx as any);
		expect(called.length).toBe(1);
		// The args ride one struct, and a trigger carries no signed route.
		expect(called[0][0]).toEqual({
			marketIndex: 3,
			orderId: 9,
			signedRoute: [],
		});

		const accounts = called[0][1].accounts;
		expect(accounts.clobMarket).toBe(clobMarket);
		// An omitted optional account encodes as the program id (anchor's `None`).
		expect(accounts.crankConditions).toBe(programId);
		expect(accounts.triggerConditions).toBe(programId);
	});
});
