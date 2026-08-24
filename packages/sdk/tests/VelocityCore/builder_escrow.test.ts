import { describe, expect, test } from 'bun:test';
import { Keypair, PublicKey } from '@solana/web3.js';
import { VelocityClient } from '../../src/velocityClient';
import { ReferrerMap } from '../../src/userMap/referrerMap';
import {
	getRevenueShareEscrowAccountPublicKey,
	getUserStatsAccountPublicKey,
} from '../../src/addresses/pda';
import { RevenueShareEscrowAccount } from '../../src/types';

// `getTakerRevenueShareAccountMetas` decides whether a perp fill must carry the
// taker's RevenueShareEscrow, and behind it the referrer's readonly UserStats that
// selects the Accelerated reward rate. It reads only `this.program.programId`,
// `this.authority` and `this.userStats`, so a minimal `this` exercises the decision.
// Regression guards for: a referred taker (escrow initialized with a referrer) whose
// order carries no builder code must still get the escrow attached, else the program
// rejects the fill with UnableToLoadRevenueShareAccount; and an explicit
// `takerReferrer` must never trigger a UserStats fetch (there is no connection here,
// so a fetch throws).
const getTakerRevenueShareAccountMetas = (VelocityClient.prototype as any)
	.getTakerRevenueShareAccountMetas as (
	takerAuthority: PublicKey,
	orderHasBuilder: boolean,
	takerEscrow?: RevenueShareEscrowAccount,
	takerIsReferred?: boolean,
	takerReferrer?: PublicKey
) => Promise<
	Array<{ pubkey: PublicKey; isWritable: boolean; isSigner: boolean }>
>;

const programId = Keypair.generate().publicKey;
// A distinct authority, so the taker is never this client's own user.
const ctx = { program: { programId }, authority: Keypair.generate().publicKey };
const escrow = (
	authority: PublicKey,
	referrer: PublicKey
): RevenueShareEscrowAccount =>
	({ authority, referrer }) as unknown as RevenueShareEscrowAccount;

describe('getTakerRevenueShareAccountMetas (fill escrow attachment)', () => {
	test('takerIsReferred, order without builder -> escrow attached', async () => {
		const authority = Keypair.generate().publicKey;
		const metas = await getTakerRevenueShareAccountMetas.call(
			ctx,
			authority,
			false,
			undefined,
			true,
			PublicKey.default
		);
		expect(metas.length).toBe(1);
		expect(
			metas[0].pubkey.equals(
				getRevenueShareEscrowAccountPublicKey(programId, authority)
			)
		).toBe(true);
		expect(metas[0].isWritable).toBe(true);
	});

	test('not referred, order without builder -> no accounts', async () => {
		const authority = Keypair.generate().publicKey;
		expect(
			await getTakerRevenueShareAccountMetas.call(
				ctx,
				authority,
				false,
				undefined,
				false
			)
		).toEqual([]);
	});

	test('builder-code order attaches escrow (no referral signal needed)', async () => {
		const authority = Keypair.generate().publicKey;
		const metas = await getTakerRevenueShareAccountMetas.call(
			ctx,
			authority,
			true,
			undefined,
			false
		);
		expect(metas.length).toBe(1);
		expect(
			metas[0].pubkey.equals(
				getRevenueShareEscrowAccountPublicKey(programId, authority)
			)
		).toBe(true);
	});

	test('decoded escrow with a referrer attaches escrow + readonly referrer stats', async () => {
		const authority = Keypair.generate().publicKey;
		const referrer = Keypair.generate().publicKey;
		const metas = await getTakerRevenueShareAccountMetas.call(
			ctx,
			authority,
			false,
			escrow(authority, referrer)
		);
		expect(metas.length).toBe(2);
		expect(
			metas[0].pubkey.equals(
				getRevenueShareEscrowAccountPublicKey(programId, authority)
			)
		).toBe(true);
		expect(
			metas[1].pubkey.equals(getUserStatsAccountPublicKey(programId, referrer))
		).toBe(true);
		// A popular referrer must not become a write-lock hotspot for every referee fill.
		expect(metas[1].isWritable).toBe(false);
	});

	test('referred taker with no known referrer still attaches the escrow alone', async () => {
		const authority = Keypair.generate().publicKey;
		const metas = await getTakerRevenueShareAccountMetas.call(
			ctx,
			authority,
			false,
			undefined,
			true,
			PublicKey.default
		);
		expect(metas.length).toBe(1);
	});

	test('escrow belonging to a different authority is rejected', async () => {
		const authority = Keypair.generate().publicKey;
		const wrong = escrow(
			Keypair.generate().publicKey,
			Keypair.generate().publicKey
		);
		expect(
			getTakerRevenueShareAccountMetas.call(ctx, authority, false, wrong)
		).rejects.toThrow();
	});
});

// Guards the referral bit ReferrerMap reads out of the taker's UserStats — the
// signal the fillers pass to getFillPerpOrderIx. Locks the `referrer_status`
// byte offset (188) and the BuilderReferral bit (0b100).
describe('ReferrerMap.mustGetIsBuilderReferral', () => {
	const REFERRER_STATUS_OFFSET = 188;
	const statsBuffer = (referrerStatus: number): Buffer => {
		const buf = Buffer.alloc(REFERRER_STATUS_OFFSET + 8);
		buf[REFERRER_STATUS_OFFSET] = referrerStatus;
		return buf;
	};
	const fakeClient = (buf: Buffer | null) =>
		({
			program: { programId: Keypair.generate().publicKey },
			connection: { getAccountInfo: async () => (buf ? { data: buf } : null) },
		}) as any;
	const authority = () => Keypair.generate().publicKey.toBase58();

	test('BuilderReferral bit set -> true', async () => {
		const map = new ReferrerMap(fakeClient(statsBuffer(0b100)));
		expect(await map.mustGetIsBuilderReferral(authority())).toBe(true);
	});

	test('IsReferred without BuilderReferral -> false', async () => {
		const map = new ReferrerMap(fakeClient(statsBuffer(0b010)));
		expect(await map.mustGetIsBuilderReferral(authority())).toBe(false);
	});
});
