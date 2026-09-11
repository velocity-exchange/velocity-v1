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
	getQuoterSlabPublicKey,
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
import { reportDispatch, sendOrPropose } from '../lib/squads';

/** Anchor default instruction discriminator: sha256("global:<name>")[..8]. */
function ixDiscriminator(name: string): Buffer {
	return createHash('sha256').update(`global:${name}`).digest().subarray(0, 8);
}

/** The relay program's `WatchV0` account length (see relay-spec). */
const WATCH_ACCOUNT_LEN = 112;

/** Relay program id (devnet + mainnet use the same deployment key). */
const DEFAULT_RELAY_PROGRAM = '4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu';

/**
 * `[disc][ClobHeaderV0 9636][len u32][pad to 8]` then the order-node arena
 * (104 bytes per node). Mirrors the CLOB program's `ORDERS_OFFSET`, which is
 * pinned there against `clob-state`'s own copy of the number.
 */
function clobMarketSpace(capacity: number): number {
	const ordersOffset = Math.ceil((8 + 9636 + 4) / 8) * 8;
	return ordersOffset + capacity * 104;
}

type ClobConfigFlags = {
	basePrecision: string;
	tickSize: string;
	stepSize: string;
	minOrderSize: string;
	blockingMinSize: string;
	defaultActivationDelay: string;
	maxActivationDelay: string;
	unknownUserGraceSlots: string;
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
		u64(flags.blockingMinSize),
		u32(Number.parseInt(flags.defaultActivationDelay, 10)),
		u32(Number.parseInt(flags.maxActivationDelay, 10)),
		u32(Number.parseInt(flags.unknownUserGraceSlots, 10)),
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
	getAccountInfo(
		key: PublicKey
	): Promise<{ data: Buffer; owner: PublicKey } | null>;
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

/** The mutable half of the book's header, as `update_market_v0` takes it. */
type ClobUpdateFlags = {
	tickSize?: string;
	stepSize?: string;
	minOrderSize?: string;
	blockingMinSize?: string;
	defaultActivationDelay?: string;
	maxActivationDelay?: string;
	unknownUserGraceSlots?: string;
	evictThreshold?: string;
	maxQuoteLevels?: string;
	maxExecuteFills?: string;
	maxExecuteUsers?: string;
	reservationGraceSlots?: string;
};

/**
 * Borsh wire of the CLOB's `UpdateMarketArgsV0`: twelve options in the order
 * the book declares them, each a presence byte followed by the value when
 * present. An absent field leaves the book's current setting alone.
 *
 * Returns `undefined` when no field is set, since that call would write
 * nothing and still pay a transaction.
 */
function clobUpdateMarketArgs(flags: ClobUpdateFlags): Buffer | undefined {
	const opt = (width: number, value?: string) => {
		if (value === undefined) {
			return Buffer.from([0]);
		}
		const b = Buffer.alloc(1 + width);
		b.writeUInt8(1, 0);
		new BN(value).toArrayLike(Buffer, 'le', width).copy(b, 1);
		return b;
	};
	const fields: Buffer[] = [
		opt(8, flags.tickSize),
		opt(8, flags.stepSize),
		opt(8, flags.minOrderSize),
		opt(8, flags.blockingMinSize),
		opt(4, flags.defaultActivationDelay),
		opt(4, flags.maxActivationDelay),
		opt(4, flags.unknownUserGraceSlots),
		opt(4, flags.evictThreshold),
		opt(2, flags.maxQuoteLevels),
		opt(2, flags.maxExecuteFills),
		opt(2, flags.maxExecuteUsers),
		opt(2, flags.reservationGraceSlots),
	];
	return fields.some((f) => f.length > 1) ? Buffer.concat(fields) : undefined;
}

/**
 * The book a perp market names, and the program that owns it.
 *
 * Both are read on chain rather than passed: the market stores its book, and
 * the book account's owner is the CLOB program deployment it lives on.
 */
async function marketBook(
	client: AccountDecoder,
	connection: RpcConnection,
	perpMarket: PublicKey,
	marketIndex: number
): Promise<{ book: PublicKey; program: PublicKey; authority: PublicKey }> {
	const marketInfo = await connection.getAccountInfo(perpMarket);
	if (!marketInfo) {
		throw new Error(`perp market ${marketIndex} not found`);
	}
	const book = new PublicKey(
		(
			client.program.coder.accounts.decode('perpMarket', marketInfo.data) as {
				clobMarket: PublicKey;
			}
		).clobMarket
	);
	if (book.equals(PublicKey.default)) {
		throw new Error(`perp market ${marketIndex} names no book`);
	}
	const bookInfo = await connection.getAccountInfo(book);
	if (!bookInfo) {
		throw new Error(`book ${book.toBase58()} not found`);
	}
	// `ClobHeaderV0.authority` is the header's first field, after the 8-byte
	// discriminator. It is the one signer `update_market_v0` accepts.
	return {
		book,
		program: bookInfo.owner,
		authority: new PublicKey(bookInfo.data.subarray(8, 40)),
	};
}

/**
 * CLOB market bring-up. One command stands a market's whole CLOB up: the
 * book account on the CLOB program, the market's quoter slab when it does
 * not exist yet, the book's velocity quoter-registry entry (registered CPI
 * surface + admin approval into the slab), the canonical-CLOB attach
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
					"Stand up a perp market's CLOB in one command: create the book account, initialize it on the CLOB program (place_authority = the market's quoter slab), create the market's quoter slab when missing, register its quoter entry and approve it into the slab, attach it as the market's canonical CLOB (creating the crank conditions + reservoir), and optionally register the relay watch and fund the reservoir. Signer must hold warm/cold admin (approval + attach). Direct-send only — fresh account keypairs must co-sign, so --multisig is rejected."
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
				'--blocking-min-size <n>',
				'floor on the size of an order that may end a fill walk when its owner is not carried; 0 disables',
				'0'
			)
			.option(
				'--default-activation-delay <slots>',
				'default taker speed bump',
				'1'
			)
			.option('--max-activation-delay <slots>', 'max caller-chosen delay', '20')
			.option(
				'--unknown-user-grace-slots <n>',
				'grace window for an order whose owner a caller does not carry: a fill walk skips such an order for this many slots after it becomes matchable, and stops on it after that. Named for the header field it writes, so it is not confused with --reservation-grace-slots on update-config',
				'2'
			)
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
				"floor on what the protocol must net from a cross-match crank, QUOTE_PRECISION (1e6). Cranking a cross pays the reservoir's keeper fee, so a cross that clears by a cent is one the protocol pays to run — and one anyone can manufacture. Must be above zero; set it to cover the cross payout with margin",
				'10000'
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
				const quoterSlab = getQuoterSlabPublicKey(
					client.program.programId,
					marketIndex
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
				// with the market's quoter slab as its place authority — the one
				// identity velocity signs every external quoter CPI as.
				// Deliberately not the vault authority: signer privilege is
				// inherited by a callee, and the vault authority moves funds.
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
						{ pubkey: quoterSlab, isSigner: false, isWritable: false },
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
				// One unified account list; each leg names its slice by index.
				// The quote leg reads the book; the execute leg also carries the
				// quoter slab the book checks velocity's CPI signature against.
				const registerAccounts =
					client.program.instruction.updateQuoterAccounts(
						{
							metas: [
								{ pubkey: book.publicKey, isWritable: true },
								{ pubkey: quoterSlab, isWritable: false },
							],
							quoteIndexes: Buffer.from([0]),
							executeIndexes: Buffer.from([0, 1]),
						},
						{ accounts: { authority: wallet, quoter: quoterPda } }
					);
				// Approval copies the staging config into the market's slab, so
				// the slab has to exist first. It is permissionless and shared by
				// every quoter on the market, so create it only when missing.
				const slabIxs = (await provider.connection.getAccountInfo(quoterSlab))
					? []
					: [
							client.program.instruction.initializeQuoterSlab(
								{ marketIndex },
								{
									accounts: {
										payer: wallet,
										perpMarket,
										quoterSlab,
										rent: SYSVAR_RENT_PUBKEY,
										systemProgram: SystemProgram.programId,
									},
								}
							),
					  ];
				// Approving an entry approves the binary behind it, so the CLOB
				// program has to be frozen and its program-data account is the
				// proof. A program on a loader that cannot upgrade in place has
				// no such account and the program is inherently fixed.
				const [clobProgramData] = PublicKey.findProgramAddressSync(
					[clobProgram.toBuffer()],
					BPF_LOADER_UPGRADEABLE_ID
				);
				const approve = client.program.instruction.updateQuoterApproved(
					{ approved: true },
					{
						accounts: {
							admin: wallet,
							state: await client.getStatePublicKey(),
							quoter: quoterPda,
							perpMarket,
							quoterSlab,
							quoterProgram: clobProgram,
							quoterProgramData: clobProgramData,
							// Approval right-sizes the slab account.
							systemProgram: SystemProgram.programId,
						},
					}
				);
				await provider.sendAndConfirm(
					new Transaction().add(
						...slabIxs,
						initQuoter,
						registerAccounts,
						approve
					)
				);
				console.log(
					`quoter ${quoterPda.toBase58()} registered + approved into slab ${quoterSlab.toBase58()}`
				);

				// 3. Attach: names the canonical CLOB and stands up the crank
				// conditions + reservoir; optionally fund the reservoir.
				const attach = client.program.instruction.updatePerpMarketClobQuoter(
					{
						crankCostUnits,
						expireFallbackSlots: new BN(flags.expireFallbackSlots),
						minCrossSurplus: new BN(flags.minCrossSurplus),
					},
					{
						accounts: {
							admin: wallet,
							state: await client.getStatePublicKey(),
							perpMarket,
							quoter: quoterPda,
							quoterSlab,
							clobMarket: book.publicKey,
							clobProgram,
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
				// The market stores its book directly.
				const book = new PublicKey(
					(
						client.program.coder.accounts.decode(
							'perpMarket',
							marketInfo.data
						) as { clobMarket: PublicKey }
					).clobMarket
				);
				if (book.equals(PublicKey.default)) {
					throw new Error(`perp market ${marketIndex} names no book`);
				}
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

	withGlobalOptions(
		clobMarket
			.command('update-config <market>')
			.description(
				"Retune an existing book's mutable config through the CLOB's update_market_v0 (the book's authority signs). Only the flags passed are written; the rest keep their current setting. base_precision, market_index and place_authority are immutable and are not offered. The book and its program are read off the perp market, so no program id is needed."
			)
			.option('--tick-size <n>', 'price tick (PRICE_PRECISION)')
			.option('--step-size <n>', 'size step (base precision)')
			.option('--min-order-size <n>', 'minimum order size (base precision)')
			.option(
				'--blocking-min-size <n>',
				'floor on the size of an order that may end a fill walk when its owner is not carried; 0 disables'
			)
			.option('--default-activation-delay <slots>', 'default taker speed bump')
			.option('--max-activation-delay <slots>', 'max caller-chosen delay')
			.option(
				'--unknown-user-grace-slots <n>',
				'grace window for an order whose owner a caller does not carry'
			)
			.option(
				'--evict-threshold <n>',
				'per-side soft cap enabling the evict crank'
			)
			.option('--max-quote-levels <n>', 'quote response level cap')
			.option('--max-execute-fills <n>', 'execute response fill cap')
			.option('--max-execute-users <n>', 'execute user-set cap')
			.option(
				'--reservation-grace-slots <n>',
				"slots past its activation slot for which a taker remainder's claim on the depth it crosses is still honoured. A claim hides that depth from every caller but the crank that owes the taker its improvement, so this is what bounds a crank that never lands. 0 ends a claim the slot its remainder activates; 150 slots is the ceiling, which is how long a transaction stays valid after its blockhash"
			)
	).action(async (market: string, flags: ClobUpdateFlags, cmd: Command) => {
		const marketIndex = Number.parseInt(market, 10);
		const args = clobUpdateMarketArgs(flags);
		if (!args) {
			throw new Error(
				'update-config writes nothing: pass at least one config flag (see --help)'
			);
		}
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts, false);
		try {
			const { book, program, authority } = await marketBook(
				client,
				provider.connection,
				getPerpMarketPublicKeySync(client.program.programId, marketIndex),
				marketIndex
			);
			const ix = new TransactionInstruction({
				programId: program,
				keys: [
					{ pubkey: book, isSigner: false, isWritable: true },
					{ pubkey: authority, isSigner: true, isWritable: false },
				],
				data: Buffer.concat([ixDiscriminator('update_market_v0'), args]),
			});
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin clob-market update-config'
			);
			reportDispatch(
				`book ${book.toBase58()} config updated (authority ${authority.toBase58()})`,
				result
			);
		} finally {
			if ((client as any).isSubscribed) {
				await client.unsubscribe();
			}
		}
	});
}
