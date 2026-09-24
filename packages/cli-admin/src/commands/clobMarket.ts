import { createHash } from 'crypto';
import { Command } from 'commander';
import {
	Keypair,
	PublicKey,
	SystemProgram,
	Transaction,
	TransactionInstruction,
} from '@solana/web3.js';
import { BN } from '@coral-xyz/anchor';
import {
	ClobUpdateMarketArgsV0,
	decodeQuoterSlab,
	getClobCrankConditionsPublicKey,
	getPerpMarketPublicKeySync,
	getQuoterPublicKey,
	getQuoterSlabPublicKey,
	QuoterType,
} from '@velocity-exchange/sdk';
import {
	readCrankCostUnits,
	withCrankCostUnitOptions,
} from '../lib/crankCostUnits';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';

import { buildAdminClient, buildProvider } from '../lib/provider';
import {
	reportDispatch,
	resolveAdminAuthority,
	sendOrPropose,
} from '../lib/squads';

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
 * (104 bytes/node). Mirrors the CLOB program's `ORDERS_OFFSET`, pinned against `clob-state`.
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
 * Instructions that register one condition block as a relay `WatchV0`. The
 * first creates the zeroed watch account. The second calls `register_watch_v0`
 * with the block's account-data offset. Relay makes registration
 * permissionless, and the registrar only gains the right to close the watch
 * and reclaim its rent.
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
 * The offset is read, not derived. `updatePerpMarketClobQuoter` gets it back
 * from the book when it registers velocity's resolvers there, so no off-chain
 * code needs to know the market account's layout.
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
 * Relay watches for both of a market's condition blocks. Velocity's conditions
 * account holds the cross fallback poll in its first field, at offset 8. The
 * CLOB market account holds the four conditions that describe the book: an
 * expired order, a side at its eviction threshold, a crossed book, and an
 * order reaching its activation slot. A watch on the velocity account alone
 * leaves the book's own cranks with nothing to wake a turner.
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
 * The CLOB's `UpdateMarketArgsV0` in the shape velocity's
 * `updatePerpMarketClobBookConfig` takes. An absent flag leaves the book's
 * current setting unchanged.
 *
 * Returns `undefined` when no field is set, because that call would write
 * nothing and still pay for a transaction.
 */
function clobUpdateMarketArgs(
	flags: ClobUpdateFlags
): ClobUpdateMarketArgsV0 | undefined {
	const big = (value?: string) => (value === undefined ? null : new BN(value));
	const num = (value?: string) =>
		value === undefined ? null : Number.parseInt(value, 10);
	const args: ClobUpdateMarketArgsV0 = {
		orderTickSize: big(flags.tickSize),
		orderStepSize: big(flags.stepSize),
		minOrderSize: big(flags.minOrderSize),
		blockingMinSize: big(flags.blockingMinSize),
		defaultActivationDelaySlots: num(flags.defaultActivationDelay),
		maxActivationDelaySlots: num(flags.maxActivationDelay),
		unknownUserGraceSlots: num(flags.unknownUserGraceSlots),
		evictThresholdPerSide: num(flags.evictThreshold),
		maxQuoteLevels: num(flags.maxQuoteLevels),
		maxExecuteFills: num(flags.maxExecuteFills),
		maxExecuteUsers: num(flags.maxExecuteUsers),
		reservationGraceSlots: num(flags.reservationGraceSlots),
	};
	return Object.values(args).some((v) => v !== null) ? args : undefined;
}

/**
 * The book a perp market names, and the program that owns it.
 *
 * Both come from chain rather than from the caller. The market stores its
 * book, and the book account's owner is the CLOB program deployment it lives
 * on.
 */
async function marketBook(
	client: AccountDecoder,
	connection: RpcConnection,
	perpMarket: PublicKey,
	marketIndex: number
): Promise<{ book: PublicKey; program: PublicKey }> {
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

	return { book, program: bookInfo.owner };
}

/**
 * The quoter entry the market's slab holds in the book's slot. Slot 0 is the
 * book by convention.
 */
async function bookQuoterEntry(
	connection: RpcConnection,
	quoterSlab: PublicKey
): Promise<PublicKey> {
	const info = await connection.getAccountInfo(quoterSlab);
	if (!info) {
		throw new Error(`quoter slab ${quoterSlab.toBase58()} not found`);
	}

	return decodeQuoterSlab(info.data).slots[0].entry;
}

/**
 * CLOB market bring-up. One command creates a market's whole CLOB. It creates
 * the book account on the CLOB program, the market's quoter slab when that
 * does not exist yet, and the book's velocity quoter-registry entry with its
 * registered CPI surface and its admin approval into the slab. It then runs
 * the canonical-CLOB attach, which also creates the relay crank conditions and
 * the reservoir. It can also register the relay watch and fund the reservoir.
 *
 * This command sends directly and cannot propose. The book and watch accounts
 * are fresh keypairs that must co-sign, and a Squads proposal cannot co-sign.
 * A multisig setup runs the admin-gated legs one at a time with
 * `quoter set-approved` and `quoter set-market-clob`.
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
					"Create a perp market's CLOB in one command. It creates the book account and initializes it on the CLOB program with the market's quoter slab as both the place authority and the config authority. It creates the market's quoter slab when that is missing, registers the quoter entry and approves it into the slab, then attaches it as the market's canonical CLOB, which also creates the crank conditions and the reservoir. It can also register the relay watch and fund the reservoir. The signer must hold the warm or cold admin role, which the approval and the attach require. This command sends directly and rejects --multisig, because fresh account keypairs must co-sign."
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
				'floor on the size of an order that may end a fill walk when its owner is not carried. 0 disables the floor',
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
				'grace window for an order whose owner a caller does not carry. A fill walk skips such an order for this many slots after it becomes matchable, and stops on it after that. Named for the header field it writes, so it is not confused with --reservation-grace-slots on update-config',
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
				"floor on what the protocol must net from a cross-match crank, QUOTE_PRECISION (1e6). Cranking a cross pays the reservoir's keeper fee, so a cross that clears by a cent costs the protocol more than it earns, and anyone can create one. Must be above zero. Set it high enough to cover the cross payout with margin",
				'10000'
			)
			.option(
				'--relay-program <pubkey>',
				`register a relay WatchV0 over the conditions block. The default relay id is ${DEFAULT_RELAY_PROGRAM}. Pass "none" to skip the registration`,
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
				const conditions = getClobCrankConditionsPublicKey(
					client.program.programId,
					marketIndex
				);
				const wallet = provider.wallet.publicKey;

				// The book is a fresh account on the CLOB program. Its place
				// authority and its config authority are both the market's quoter
				// slab, the one identity velocity signs every external quoter CPI
				// as. The attach refuses a book with any other config authority.
				// The vault authority is not used here, because a callee inherits
				// signer privilege and the vault authority moves funds.
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
						{ pubkey: quoterSlab, isSigner: false, isWritable: false },
						{ pubkey: quoterSlab, isSigner: false, isWritable: false },
						{ pubkey: book.publicKey, isSigner: true, isWritable: true },
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

				const quoterPda = getQuoterPublicKey(
					client.program.programId,
					marketIndex,
					clobProgram,
					quoterUser
				);
				const initQuoter = await client.getInitializeQuoterIx(
					marketIndex,
					{
						quoterType: QuoterType.CLOB,
						responseAccount: book.publicKey,
						quoteV0Discriminator: Array.from(ixDiscriminator('quote_v0')),
						// The book answers who rests on it through this leg, so a
						// router never has to decode the book to build a fill.
						quoteL3V0Discriminator: Array.from(ixDiscriminator('quote_l3_v0')),
						executeV0Discriminator: Array.from(ixDiscriminator('execute_v0')),
					},

					clobProgram,
					quoterUser,
					wallet
				);

				// One unified account list, and each leg names its slice by index.
				// The quote leg reads the book. The execute leg also carries the
				// quoter slab, which the book checks velocity's CPI signature
				// against.
				const registerAccounts = await client.getUpdateQuoterAccountsIx(
					quoterPda,
					[
						{ pubkey: book.publicKey, isWritable: true },
						{ pubkey: quoterSlab, isWritable: false },
					],

					[0],
					[0, 1],
					wallet
				);

				// Registration reads the slab and approval writes it, so the slab
				// must exist before either one. It is permissionless and shared by
				// every quoter on the market, so it is created only when missing.
				// The creation comes first in the transaction below.
				const slabIxs = (await provider.connection.getAccountInfo(quoterSlab))
					? []
					: [await client.getInitializeQuoterSlabIx(marketIndex, wallet)];

				const approve = await client.getUpdateQuoterApprovedIx(
					quoterPda,
					true,
					marketIndex,
					clobProgram,
					book.publicKey,
					wallet
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

				// The attach names the canonical CLOB and creates the crank
				// conditions and the reservoir.
				const attach = await client.getUpdatePerpMarketClobQuoterIx(
					marketIndex,
					quoterPda,
					book.publicKey,
					clobProgram,
					{
						crankCostUnits,
						expireFallbackSlots: new BN(flags.expireFallbackSlots),
						minCrossSurplus: new BN(flags.minCrossSurplus),
					},

					wallet
				);

				await provider.sendAndConfirm(new Transaction().add(attach));
				// The reservoir deliberately holds rent and nothing more. A market
				// funds itself from the crank treasury through the refill crank.
				// A manual transfer here would leave the reservoir's mirrored
				// balance behind the lamports it really holds.
				console.log(
					`attached as perp-market[${marketIndex}].clob_quoter; conditions ${conditions.toBase58()}; the crank treasury refills it`
				);

				// The relay watches let turners discover both condition blocks,
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
				"Register a relay WatchV0 over an existing market's crank-conditions block, so relay turners discover its CLOB crank work. Relay makes registration permissionless. The signing wallet becomes the registrar and can later close the watch to reclaim its rent. This command sends directly and cannot propose."
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
				"Change an existing book's mutable config through velocity's update_perp_market_clob_book_config, which CPIs the CLOB's update_market_v0 as the book's config authority and rewrites velocity's copy of the book's rules in the same instruction. The signer must hold the warm or cold admin role. Only the flags passed are written, and the rest keep their current setting. base_precision, market_index and place_authority are immutable, so this command does not offer them. The book, its program and its quoter entry come from chain, so no program id is needed."
			)
			.option('--tick-size <n>', 'price tick (PRICE_PRECISION)')
			.option('--step-size <n>', 'size step (base precision)')
			.option('--min-order-size <n>', 'minimum order size (base precision)')
			.option(
				'--blocking-min-size <n>',
				'floor on the size of an order that may end a fill walk when its owner is not carried. 0 disables the floor'
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
				"slots past its activation slot for which a taker remainder's claim on the depth it crosses still holds. A claim hides that depth from every caller except the crank that owes the taker its improvement, so this value bounds a crank that never lands. 0 ends a claim in the slot its remainder activates. 150 slots is the ceiling, which is how long a transaction stays valid after its blockhash"
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
			const { book, program } = await marketBook(
				client,
				provider.connection,
				getPerpMarketPublicKeySync(client.program.programId, marketIndex),
				marketIndex
			);
			const quoter = await bookQuoterEntry(
				provider.connection,
				getQuoterSlabPublicKey(client.program.programId, marketIndex)
			);
			const admin = resolveAdminAuthority(
				provider,
				opts.multisig ? new PublicKey(opts.multisig) : undefined
			);
			const ix = await client.getUpdatePerpMarketClobBookConfigIx(
				marketIndex,
				quoter,
				book,
				program,
				args,
				admin
			);
			const result = await sendOrPropose(
				provider,
				[ix],
				opts.multisig ? new PublicKey(opts.multisig) : undefined,
				'velocity-admin clob-market update-config'
			);

			reportDispatch(`book ${book.toBase58()} config updated`, result);
		} finally {
			if ((client as any).isSubscribed) {
				await client.unsubscribe();
			}
		}
	});

	withGlobalOptions(
		clobMarket
			.command('resize <market> <newCapacity>')
			.description(
				"Grow an existing book's order arena through velocity's resize_perp_market_clob_book, which CPIs the CLOB's resize_market_v0 as the book's config authority. The signer must hold the warm or cold admin role and pays the extra rent. One call grows the arena by at most 10KB, about 116 orders, so a large target takes repeated calls. The arena never shrinks."
			)
	).action(
		async (market: string, newCapacity: string, _flags, cmd: Command) => {
			const marketIndex = Number.parseInt(market, 10);
			const opts = readGlobalOpts(cmd);
			const provider = buildProvider(opts);
			const client = await buildAdminClient(opts, false);
			try {
				const { book, program } = await marketBook(
					client,
					provider.connection,
					getPerpMarketPublicKeySync(client.program.programId, marketIndex),
					marketIndex
				);
				const admin = resolveAdminAuthority(
					provider,
					opts.multisig ? new PublicKey(opts.multisig) : undefined
				);
				const ix = await client.getResizePerpMarketClobBookIx(
					marketIndex,
					book,
					program,
					Number.parseInt(newCapacity, 10),
					admin
				);
				const result = await sendOrPropose(
					provider,
					[ix],
					opts.multisig ? new PublicKey(opts.multisig) : undefined,
					'velocity-admin clob-market resize'
				);

				reportDispatch(
					`book ${book.toBase58()} resized to ${newCapacity}`,
					result
				);
			} finally {
				if ((client as any).isSubscribed) {
					await client.unsubscribe();
				}
			}
		}
	);
}
