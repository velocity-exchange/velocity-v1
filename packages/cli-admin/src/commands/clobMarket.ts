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
	getCrankTreasuryPublicKey,
	getClobCrankConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getClobAuthorityPublicKey,
	QuoterCpiLeg,
	QuoterType,
} from '@velocity-exchange/sdk';
import {
	readCrankCostUnits,
	withCrankCostUnitOptions,
} from '../lib/crankCostUnits';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';

/** `BPFLoaderUpgradeab1e11111111111111111111111`, which owns a program's data account. */
const BPF_LOADER_UPGRADEABLE_ID = new PublicKey(
	'BPFLoaderUpgradeab1e11111111111111111111111'
);
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
 * Instructions registering one condition block as a relay `WatchV0`: create
 * the zeroed watch account, then `register_watch_v0` pointing at the block's
 * account-data offset. Registration is permissionless on relay's side — the
 * registrar only gains the right to close the watch and reclaim its rent.
 *
 * A market has two blocks, and both need a watch. Velocity's conditions
 * account holds the cross fallback poll, at offset 8 (the block is its first
 * field). The CLOB market account holds the four conditions that describe the
 * book itself — an expired order, a side at its eviction threshold, a crossed
 * book, an order reaching its activation slot — at whatever offset
 * `updatePerpMarketClobQuoter` reported when it registered velocity's
 * resolvers there. Watch only the first and the book's own cranks never fire.
 */
async function watchRegistrationIxs(
	connection: { getMinimumBalanceForRentExemption(n: number): Promise<number> },
	payer: PublicKey,
	relayProgram: PublicKey,
	target: PublicKey,
	watch: Keypair,
	blockOffset: number
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
	offsetArg.writeUInt32LE(blockOffset);
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

/** Just enough of a connection to fetch an account and price its rent. */
type RpcConnection = {
	getAccountInfo(key: PublicKey): Promise<{ data: Buffer } | null>;
	getMinimumBalanceForRentExemption(n: number): Promise<number>;
};

/** Just enough of the client to decode a velocity account by name. */
type AccountDecoder = {
	program: {
		coder: { accounts: { decode(name: string, data: Buffer): unknown } };
	};
};

/**
 * The account-data offset of the book's own condition block, as the market's
 * attach recorded it.
 *
 * Read rather than derived: `updatePerpMarketClobQuoter` gets it back from
 * the book when it registers velocity's resolvers there, so nothing off chain
 * has to know the market account's layout.
 */
async function clobBlockOffset(
	client: AccountDecoder,
	connection: RpcConnection,
	conditions: PublicKey
): Promise<number> {
	const info = await connection.getAccountInfo(conditions);
	if (!info) {
		throw new Error(`crank conditions ${conditions.toBase58()} not found`);
	}
	const decoded = client.program.coder.accounts.decode(
		'clobCrankConditionsV0',
		info.data
	) as { clobBlockOffset: number };
	if (!decoded.clobBlockOffset) {
		throw new Error(
			`crank conditions ${conditions.toBase58()} carry no book block offset; re-run the market attach`
		);
	}
	return decoded.clobBlockOffset;
}

/**
 * Both of a market's condition blocks, registered as relay watches: velocity's
 * conditions account (its block is the first field, at offset 8) and the CLOB
 * market account (the four conditions describing the book). Watching only the
 * first leaves the book's own cranks — expiry, eviction, a crossed book, an
 * activation coming due — with nothing to wake a turner.
 */
async function marketWatchIxs(
	client: AccountDecoder,
	connection: RpcConnection,
	payer: PublicKey,
	relayProgram: PublicKey,
	conditions: PublicKey,
	clobMarket: PublicKey,
	watches: [Keypair, Keypair]
): Promise<TransactionInstruction[]> {
	const bookOffset = await clobBlockOffset(client, connection, conditions);
	return [
		...(await watchRegistrationIxs(
			connection,
			payer,
			relayProgram,
			conditions,
			watches[0],
			8
		)),
		...(await watchRegistrationIxs(
			connection,
			payer,
			relayProgram,
			clobMarket,
			watches[1],
			bookOffset
		)),
	];
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
		withCrankCostUnitOptions(
			clobMarket
				.command('init <market>')
				.description(
					"Stand up a perp market's CLOB in one command: create the book account, initialize it on the CLOB program (place_authority = the quoter CPI signer), register + approve its quoter entry, attach it as the market's canonical CLOB (creating the crank conditions + reservoir), and optionally register the relay watch and fund the reservoir. Signer must hold warm/cold admin (approval + attach). Direct-send only — fresh account keypairs must co-sign, so --multisig is rejected."
				)
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
				'--min-cross-surplus <quote>',
				"floor on what the protocol must net from a cross-match crank, QUOTE_PRECISION (1e6). Cranking a cross pays the reservoir's keeper fee, so a cross that clears by a cent is one worth declining",
				'0'
			)
			.option(
				'--relay-program <pubkey>',
				`register a relay WatchV0 over the conditions block (default relay id ${DEFAULT_RELAY_PROGRAM}; pass "none" to skip)`,
				DEFAULT_RELAY_PROGRAM
			)
	).action(
		async (
			market: string,
			flags: ClobConfigFlags & {
				clobProgram: string;
				capacity: string;
				quoterUser: string;
				expireFallbackSlots: string;
				minCrossSurplus: string;
				relayProgram: string;
			},
			cmd: Command
		) => {
			const crankCostUnits = readCrankCostUnits(
				flags as unknown as Record<string, string | undefined>
			);
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
				const clobAuthority = getClobAuthorityPublicKey(
					client.program.programId
				);
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
				// with velocity's CLOB place authority. Deliberately its own key —
				// not the vault authority, and not the per-entry signer a
				// third-party quoter is handed. Signer privilege is inherited by a
				// callee, and this key may place and cancel on any book for any
				// user, so nothing outside velocity ever receives it.
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
						{ pubkey: clobAuthority, isSigner: false, isWritable: false },
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
						// The book answers who rests on it through this leg, so a
						// router never has to decode the book to build a fill.
						quoteL3V0Discriminator: Array.from(ixDiscriminator('quote_l3_v0')),
						executeV0Discriminator: Array.from(ixDiscriminator('execute_v0')),
					},
					{
						accounts: {
							state: await client.getStatePublicKey(),
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
				// Approving an entry approves the binary behind it, so the CLOB
				// program has to be frozen and its program-data account is the
				// proof. A program on a loader that cannot upgrade in place has
				// no such account and the program is inherently fixed.
				const [clobProgramData] = PublicKey.findProgramAddressSync(
					[clobProgram.toBuffer()],
					BPF_LOADER_UPGRADEABLE_ID
				);
				const approve = client.program.instruction.updateQuoterApproved(true, {
					accounts: {
						admin: wallet,
						state: await client.getStatePublicKey(),
						quoter: quoterPda,
						quoterProgram: clobProgram,
						quoterProgramData: clobProgramData,
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
							{ pubkey: clobAuthority, isWritable: false },
						]),
						approve
					)
				);
				console.log(`quoter ${quoterPda.toBase58()} registered + approved`);

				// 3. Attach: names the canonical CLOB and stands up the crank
				// conditions + reservoir; optionally fund the reservoir.
				const attach = client.program.instruction.updatePerpMarketClobQuoter(
					crankCostUnits,
					new BN(flags.expireFallbackSlots),
					new BN(flags.minCrossSurplus),
					{
						accounts: {
							admin: wallet,
							state: await client.getStatePublicKey(),
							perpMarket,
							quoter: quoterPda,
							clobMarket: book.publicKey,
							clobProgram,
							clobAuthority: client.getClobAuthorityPublicKey(),
							crankConditions: conditions,
							treasury: getCrankTreasuryPublicKey(client.program.programId),
							rent: SYSVAR_RENT_PUBKEY,
							systemProgram: SystemProgram.programId,
						},
					}
				);
				await provider.sendAndConfirm(new Transaction().add(attach));
				// The reservoir is left holding rent alone on purpose. A market
				// funds itself from the crank treasury through the refill crank,
				// and a hand transfer here would only leave the reservoir's
				// mirrored balance behind what it really holds.
				console.log(
					`attached as perp-market[${marketIndex}].clob_quoter; conditions ${conditions.toBase58()}; the crank treasury refills it`
				);

				// 4. Relay watches, so turners discover both condition blocks:
				// velocity's and the book's own.
				if (flags.relayProgram.toLowerCase() !== 'none') {
					const relayProgram = new PublicKey(flags.relayProgram);
					const watches: [Keypair, Keypair] = [
						Keypair.generate(),
						Keypair.generate(),
					];
					const ixs = await marketWatchIxs(
						client,
						provider.connection,
						wallet,
						relayProgram,
						conditions,
						book.publicKey,
						watches
					);
					await provider.sendAndConfirm(new Transaction().add(...ixs), watches);
					console.log(
						`relay watches registered: ${watches[0].publicKey.toBase58()} -> conditions, ` +
							`${watches[1].publicKey.toBase58()} -> book`
					);
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
				const perpMarket = getPerpMarketPublicKeySync(
					client.program.programId,
					marketIndex
				);
				const marketInfo = await provider.connection.getAccountInfo(perpMarket);
				if (!marketInfo) {
					throw new Error(`perp market ${marketIndex} not found`);
				}
				const clobMarket = new PublicKey(
					(
						client.program.coder.accounts.decode(
							'perpMarket',
							marketInfo.data
						) as { clobQuoter: PublicKey }
					).clobQuoter
				);
				const entryInfo = await provider.connection.getAccountInfo(clobMarket);
				if (!entryInfo) {
					throw new Error(
						`clob quoter entry ${clobMarket.toBase58()} not found`
					);
				}
				const book = new PublicKey(
					(
						client.program.coder.accounts.decode(
							'quoterV0',
							entryInfo.data
						) as { responseAccount: PublicKey }
					).responseAccount
				);
				const watches: [Keypair, Keypair] = [
					Keypair.generate(),
					Keypair.generate(),
				];
				const ixs = await marketWatchIxs(
					client,
					provider.connection,
					provider.wallet.publicKey,
					new PublicKey(flags.relayProgram),
					conditions,
					book,
					watches
				);
				await provider.sendAndConfirm(new Transaction().add(...ixs), watches);
				console.log(
					`relay watches: ${watches[0].publicKey.toBase58()} -> conditions ${conditions.toBase58()} (offset 8), ` +
						`${watches[1].publicKey.toBase58()} -> book ${book.toBase58()}`
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);
}
