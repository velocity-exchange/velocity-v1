import { createHash } from 'crypto';
import { Command } from 'commander';
import {
	Keypair,
	PublicKey,
	SystemProgram,
	SYSVAR_RENT_PUBKEY,
	Transaction,
	TransactionInstruction,
} from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	getClobCrankConditionsPublicKey,
	getPerpMarketPublicKeySync,
	QuoterCpiLeg,
	QuoterType,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';

/** Anchor default instruction discriminator: sha256("global:<name>")[..8]. */
function ixDiscriminator(name: string): Buffer {
	return createHash('sha256').update(`global:${name}`).digest().subarray(0, 8);
}

/** The relay program's `WatchV0` account length (see relay-spec). */
const WATCH_ACCOUNT_LEN = 112;

/** Relay program id (devnet + mainnet use the same deployment key). */
const DEFAULT_RELAY_PROGRAM = '4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu';

/**
 * `[disc][ClobHeaderV0 8352][len u32][pad to 8]` then the order-node arena
 * (96 bytes per node). Mirrors the CLOB program's `ORDERS_OFFSET`.
 */
function clobMarketSpace(capacity: number): number {
	const ordersOffset = Math.ceil((8 + 8352 + 4) / 8) * 8;
	return ordersOffset + capacity * 96;
}

type ClobConfigFlags = {
	basePrecision: string;
	tickSize: string;
	stepSize: string;
	minOrderSize: string;
	defaultActivationDelay: string;
	maxActivationDelay: string;
	graceSlots: string;
	evictThreshold: string;
	maxQuoteLevels: string;
	maxExecuteFills: string;
	maxExecuteUsers: string;
};

/** Borsh wire of the CLOB's `MarketConfigV0`. */
function clobMarketConfig(marketIndex: number, flags: ClobConfigFlags): Buffer {
	const u16 = (v: number) => {
		const b = Buffer.alloc(2);
		b.writeUInt16LE(v);
		return b;
	};
	const u32 = (v: number) => {
		const b = Buffer.alloc(4);
		b.writeUInt32LE(v);
		return b;
	};
	const u64 = (v: string) => new BN(v).toArrayLike(Buffer, 'le', 8);
	return Buffer.concat([
		u16(marketIndex),
		u64(flags.basePrecision),
		u64(flags.tickSize),
		u64(flags.stepSize),
		u64(flags.minOrderSize),
		u32(Number.parseInt(flags.defaultActivationDelay, 10)),
		u32(Number.parseInt(flags.maxActivationDelay, 10)),
		u32(Number.parseInt(flags.graceSlots, 10)),
		u32(Number.parseInt(flags.evictThreshold, 10)),
		u16(Number.parseInt(flags.maxQuoteLevels, 10)),
		u16(Number.parseInt(flags.maxExecuteFills, 10)),
		u16(Number.parseInt(flags.maxExecuteUsers, 10)),
	]);
}

/**
 * Instructions registering the market's crank-conditions account as a relay
 * `WatchV0`: create the zeroed watch account, then `register_watch_v0`
 * pointing at the condition block (account-data offset 8). Registration is
 * permissionless on relay's side — the registrar only gains the right to
 * close the watch and reclaim its rent.
 */
async function watchRegistrationIxs(
	connection: { getMinimumBalanceForRentExemption(n: number): Promise<number> },
	payer: PublicKey,
	relayProgram: PublicKey,
	target: PublicKey,
	watch: Keypair
): Promise<TransactionInstruction[]> {
	const rent = await connection.getMinimumBalanceForRentExemption(
		WATCH_ACCOUNT_LEN
	);
	const create = SystemProgram.createAccount({
		fromPubkey: payer,
		newAccountPubkey: watch.publicKey,
		lamports: rent,
		space: WATCH_ACCOUNT_LEN,
		programId: relayProgram,
	});
	const offsetArg = Buffer.alloc(4);
	offsetArg.writeUInt32LE(8); // the block is the account's first field, past the discriminator
	const register = new TransactionInstruction({
		programId: relayProgram,
		keys: [
			{ pubkey: payer, isSigner: true, isWritable: false },
			{ pubkey: target, isSigner: false, isWritable: false },
			{ pubkey: watch.publicKey, isSigner: false, isWritable: true },
		],
		data: Buffer.concat([ixDiscriminator('register_watch_v0'), offsetArg]),
	});
	return [create, register];
}

/**
 * CLOB market bring-up. One command stands a market's whole CLOB up: the
 * book account on the CLOB program, its velocity quoter-registry entry
 * (registered CPI surface + admin approval), the canonical-CLOB attach
 * (which also creates the relay crank conditions + reservoir), and
 * optionally the relay watch + reservoir funding.
 *
 * Direct-send only: the book and watch accounts are fresh keypairs that
 * must co-sign, which a Squads proposal cannot do. Multisig setups run the
 * admin-gated legs individually (`quoter set-approved`,
 * `quoter set-market-clob`).
 */
export function registerClobMarket(parent: Command): void {
	const clobMarket = parent
		.command('clob-market')
		.description(
			"CLOB market bring-up: create + register a perp market's order book and its relay crank plumbing."
		);

	withGlobalOptions(
		clobMarket
			.command('init <market> <keeperPaymentLamports>')
			.description(
				"Stand up a perp market's CLOB in one command: create the book account, initialize it on the CLOB program (place_authority = the velocity signer), register + approve its quoter entry, attach it as the market's canonical CLOB (creating the crank conditions + reservoir), and optionally register the relay watch and fund the reservoir. Signer must hold warm/cold admin (approval + attach). Direct-send only — fresh account keypairs must co-sign, so --multisig is rejected."
			)
			.requiredOption('--clob-program <pubkey>', 'deployed CLOB program id')
			.option('--capacity <n>', 'order-node arena capacity', '4096')
			.option(
				'--quoter-user <pubkey>',
				'quoter PDA user seed (unused for CLOB-type entries)',
				PublicKey.default.toBase58()
			)
			.option('--base-precision <n>', 'base units per whole unit', '1000000000')
			.option('--tick-size <n>', 'price tick (PRICE_PRECISION)', '100')
			.option('--step-size <n>', 'size step (base precision)', '100000')
			.option(
				'--min-order-size <n>',
				'minimum order size (base precision)',
				'100000'
			)
			.option(
				'--default-activation-delay <slots>',
				'default taker speed bump',
				'1'
			)
			.option('--max-activation-delay <slots>', 'max caller-chosen delay', '20')
			.option('--grace-slots <n>', 'unknown-user grace window', '2')
			.option(
				'--evict-threshold <n>',
				'per-side soft cap enabling the evict crank',
				'3072'
			)
			.option('--max-quote-levels <n>', 'quote response level cap', '128')
			.option('--max-execute-fills <n>', 'execute response fill cap', '64')
			.option('--max-execute-users <n>', 'execute user-set cap', '32')
			.option(
				'--expire-fallback-slots <n>',
				'expire fallback poll interval (slots)',
				'1500'
			)
			.option(
				'--fund-reservoir <lamports>',
				'lamports transferred to the conditions reservoir after the attach'
			)
			.option(
				'--relay-program <pubkey>',
				`register a relay WatchV0 over the conditions block (default relay id ${DEFAULT_RELAY_PROGRAM}; pass "none" to skip)`,
				DEFAULT_RELAY_PROGRAM
			)
	).action(
		async (
			market: string,
			keeperPaymentLamports: string,
			flags: ClobConfigFlags & {
				clobProgram: string;
				capacity: string;
				quoterUser: string;
				expireFallbackSlots: string;
				fundReservoir?: string;
				relayProgram: string;
			},
			cmd: Command
		) => {
			const marketIndex = Number.parseInt(market, 10);
			const opts = readGlobalOpts(cmd);
			if (opts.multisig) {
				throw new Error(
					'clob-market init is direct-send only (fresh account keypairs must co-sign). ' +
						'For multisig admin keys, run the steps individually: quoter init / update-accounts / set-approved / set-market-clob.'
				);
			}
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const clobProgram = new PublicKey(flags.clobProgram);
				const quoterUser = new PublicKey(flags.quoterUser);
				const velocitySigner = client.getSignerPublicKey();
				const perpMarket = getPerpMarketPublicKeySync(
					client.program.programId,
					marketIndex
				);
				const conditions = getClobCrankConditionsPublicKey(
					client.program.programId,
					marketIndex
				);
				const wallet = provider.wallet.publicKey;

				// 1. The book: a fresh account on the CLOB program, initialized
				// with velocity's signer as its place_authority.
				const book = Keypair.generate();
				const space = clobMarketSpace(Number.parseInt(flags.capacity, 10));
				const bookRent =
					await provider.connection.getMinimumBalanceForRentExemption(space);
				const createBook = SystemProgram.createAccount({
					fromPubkey: wallet,
					newAccountPubkey: book.publicKey,
					lamports: bookRent,
					space,
					programId: clobProgram,
				});
				const initBook = new TransactionInstruction({
					programId: clobProgram,
					keys: [
						{ pubkey: wallet, isSigner: true, isWritable: false },
						{ pubkey: velocitySigner, isSigner: false, isWritable: false },
						{ pubkey: book.publicKey, isSigner: false, isWritable: true },
					],
					data: Buffer.concat([
						ixDiscriminator('initialize_market_v0'),
						clobMarketConfig(marketIndex, flags),
					]),
				});
				await provider.sendAndConfirm(
					new Transaction().add(createBook, initBook),
					[book]
				);
				console.log(
					`book ${book.publicKey.toBase58()} initialized (${space} bytes)`
				);

				// 2. Registry entry: init, register the CPI surface, approve.
				const quoterPda = PublicKey.findProgramAddressSync(
					[
						Buffer.from('quoter'),
						new BN(marketIndex).toArrayLike(Buffer, 'le', 2),
						clobProgram.toBuffer(),
						quoterUser.toBuffer(),
					],
					client.program.programId
				)[0];
				const initQuoter = client.program.instruction.initializeQuoter(
					{
						marketIndex,
						quoterType: QuoterType.CLOB,
						responseAccount: book.publicKey,
						quoteV0Discriminator: Array.from(ixDiscriminator('quote_v0')),
						executeV0Discriminator: Array.from(ixDiscriminator('execute_v0')),
					},
					{
						accounts: {
							payer: wallet,
							authority: wallet,
							quoter: quoterPda,
							perpMarket,
							quoterProgram: clobProgram,
							user: quoterUser,
							rent: SYSVAR_RENT_PUBKEY,
							systemProgram: SystemProgram.programId,
						},
					}
				);
				const legAccounts = (
					leg: QuoterCpiLeg,
					metas: { pubkey: PublicKey; isWritable: boolean }[]
				) =>
					client.program.instruction.updateQuoterAccounts(
						{ leg, index: 0, metas },
						{ accounts: { authority: wallet, quoter: quoterPda } }
					);
				const approve = client.program.instruction.updateQuoterApproved(true, {
					accounts: {
						admin: wallet,
						state: await client.getStatePublicKey(),
						quoter: quoterPda,
					},
				});
				await provider.sendAndConfirm(
					new Transaction().add(
						initQuoter,
						legAccounts(QuoterCpiLeg.QUOTE, [
							{ pubkey: book.publicKey, isWritable: true },
						]),
						legAccounts(QuoterCpiLeg.EXECUTE, [
							{ pubkey: book.publicKey, isWritable: true },
							{ pubkey: velocitySigner, isWritable: false },
						]),
						approve
					)
				);
				console.log(`quoter ${quoterPda.toBase58()} registered + approved`);

				// 3. Attach: names the canonical CLOB and stands up the crank
				// conditions + reservoir; optionally fund the reservoir.
				const attach = client.program.instruction.updatePerpMarketClobQuoter(
					new BN(keeperPaymentLamports),
					new BN(flags.expireFallbackSlots),
					{
						accounts: {
							admin: wallet,
							state: await client.getStatePublicKey(),
							perpMarket,
							quoter: quoterPda,
							clobMarket: book.publicKey,
							crankConditions: conditions,
							rent: SYSVAR_RENT_PUBKEY,
							systemProgram: SystemProgram.programId,
						},
					}
				);
				const attachTx = new Transaction().add(attach);
				if (flags.fundReservoir) {
					attachTx.add(
						SystemProgram.transfer({
							fromPubkey: wallet,
							toPubkey: conditions,
							lamports: BigInt(flags.fundReservoir),
						})
					);
				}
				await provider.sendAndConfirm(attachTx);
				console.log(
					`attached as perp-market[${marketIndex}].clob_quoter; conditions ${conditions.toBase58()}` +
						(flags.fundReservoir
							? ` funded with ${flags.fundReservoir} lamports`
							: '')
				);

				// 4. Relay watch, so turners discover the conditions block.
				if (flags.relayProgram.toLowerCase() !== 'none') {
					const relayProgram = new PublicKey(flags.relayProgram);
					const watch = Keypair.generate();
					const ixs = await watchRegistrationIxs(
						provider.connection,
						wallet,
						relayProgram,
						conditions,
						watch
					);
					await provider.sendAndConfirm(new Transaction().add(...ixs), [watch]);
					console.log(`relay watch ${watch.publicKey.toBase58()} registered`);
				}
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);

	withGlobalOptions(
		clobMarket
			.command('register-watch <market>')
			.description(
				"Register a relay WatchV0 over an existing market's crank-conditions block, so relay turners discover its CLOB crank work. Permissionless on relay's side; the signing wallet becomes the registrar (and can later close the watch to reclaim rent). Direct-send only."
			)
			.option(
				'--relay-program <pubkey>',
				'relay program id',
				DEFAULT_RELAY_PROGRAM
			)
	).action(
		async (market: string, flags: { relayProgram: string }, cmd: Command) => {
			const marketIndex = Number.parseInt(market, 10);
			const opts = readGlobalOpts(cmd);
			if (opts.multisig) {
				throw new Error(
					'register-watch is direct-send only (the fresh watch keypair must co-sign); registration is permissionless, so no multisig is needed.'
				);
			}
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const conditions = getClobCrankConditionsPublicKey(
					client.program.programId,
					marketIndex
				);
				const watch = Keypair.generate();
				const ixs = await watchRegistrationIxs(
					provider.connection,
					provider.wallet.publicKey,
					new PublicKey(flags.relayProgram),
					conditions,
					watch
				);
				await provider.sendAndConfirm(new Transaction().add(...ixs), [watch]);
				console.log(
					`relay watch ${watch.publicKey.toBase58()} -> conditions ${conditions.toBase58()} (offset 8)`
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);
}
