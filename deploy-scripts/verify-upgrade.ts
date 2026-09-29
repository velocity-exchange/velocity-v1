/**
 * allow-verbose: the usage header of an operator script.
 *
 * Check that every velocity account is readable by the deployed program after an upgrade and
 * `migrate.ts`. Exits non-zero on any failure.
 *
 *   bun run deploy-scripts/verify-upgrade.ts --url <rpc> --keypair <extension authority> \
 *     [--watch-creators <pk,pk>]
 *
 * A user's relay watch counts only when the keypair, or a key `--watch-creators` names, created it
 * at the block offset. Pass the migration payer there when it is not the keypair.
 *
 * The size check simulates `extend_account` on each account and fails when the simulation would
 * grow it. The target size comes from the deployed binary, so the check does not trust the size
 * table in `migrate.ts`.
 */
import * as fs from 'fs';
import { AnchorProvider, BorshAccountsCoder, Program } from '@coral-xyz/anchor';
import {
	Connection,
	Keypair,
	PublicKey,
	SystemProgram,
	TransactionMessage,
	VersionedTransaction,
} from '@solana/web3.js';
import {
	decodeSignedMsgUserOrdersAccount,
	decodeUser,
	getClobCrankConditionsPublicKey,
	getCrankTreasuryPublicKey,
	getQuoterSlabPublicKey,
	getRelayScratchPublicKey,
	getUserAccountPublicKeySync,
	getUserStatsAccountPublicKey,
	getVelocitySignerPublicKey,
	getVelocityStateAccountPublicKey,
	positionIsAvailable,
	Wallet,
} from '@velocity-exchange/sdk';
import {
	describeImpostor,
	parseWatchCreators,
	watchesOnTarget,
} from './relay-watch';

/** Offset of the relay block in a velocity conditions account. */
const BLOCK_OFFSET = 8;

/** Types a client reads through the SDK's own decoder rather than the IDL coder.
 * `SignedMsgUserOrders` keeps a legacy layout the coder cannot read until the
 * program migrates the account on its next write. */
const SDK_DECODERS: Record<string, (data: Buffer) => unknown> = {
	User: decodeUser,
	SignedMsgUserOrders: decodeSignedMsgUserOrdersAccount,
};

type TypeReport = {
	count: number;
	undecodable: string[];
	undersized: string[];
};

class Verifier {
	readonly reports = new Map<string, TypeReport>();
	readonly failures: string[] = [];
	private names = new Map<string, string>();
	private coder: BorshAccountsCoder;

	constructor(
		private connection: Connection,
		private program: Program,
		private authority: Keypair,
		private state: PublicKey,
		private watchCreators: PublicKey[]
	) {
		this.coder = new BorshAccountsCoder(program.rawIdl);
		for (const account of (program.rawIdl as any).accounts) {
			this.names.set(
				Buffer.from(account.discriminator).toString('hex'),
				account.name
			);
		}
	}

	async checkAccount(pubkey: PublicKey, data: Buffer): Promise<void> {
		const name = this.names.get(data.subarray(0, 8).toString('hex'));
		if (!name) {
			this.failures.push(`${pubkey.toBase58()}: unknown discriminator`);
			return;
		}

		const report = this.reportFor(name);
		report.count += 1;
		try {
			(
				SDK_DECODERS[name] ??
				((bytes: Buffer) => this.coder.decode(name, bytes))
			)(data);
		} catch (error) {
			report.undecodable.push(`${pubkey.toBase58()}: ${error}`);
		}

		const grownLength = await this.simulateExtend(pubkey, data.length);
		if (grownLength > data.length) {
			report.undersized.push(
				`${pubkey.toBase58()}: ${data.length} -> ${grownLength}`
			);
		}
	}

	/** The length `extend_account` would leave the account at. A type the instruction does not
	 * extend keeps its length. */
	private async simulateExtend(
		account: PublicKey,
		length: number
	): Promise<number> {
		const ix = await this.program.methods
			.extendAccount()
			.accountsStrict({
				state: this.state,
				payer: this.authority.publicKey,
				authority: this.authority.publicKey,
				account,
				systemProgram: SystemProgram.programId,
			})
			.instruction();
		const message = new TransactionMessage({
			payerKey: this.authority.publicKey,
			recentBlockhash: PublicKey.default.toBase58(),
			instructions: [ix],
		}).compileToV0Message();

		const { value } = await this.connection.simulateTransaction(
			new VersionedTransaction(message),
			{
				sigVerify: false,
				replaceRecentBlockhash: true,
				accounts: { addresses: [account.toBase58()], encoding: 'base64' },
			}
		);

		if (value.err) {
			if (value.logs?.some((line) => line.includes('InvalidAccountExtension')))
				return length;
			this.failures.push(
				`${account.toBase58()}: extend_account simulation failed: ${JSON.stringify(
					value.err
				)}`
			);
			return length;
		}

		const post = value.accounts?.[0];
		return post ? Buffer.from(post.data[0], 'base64').length : length;
	}

	async requireExists(label: string, pubkey: PublicKey): Promise<void> {
		if (!(await this.connection.getAccountInfo(pubkey))) {
			this.failures.push(`${label} ${pubkey.toBase58()} does not exist`);
		}
	}

	/** A watch counts only when it serves the target. See `relay-watch.ts`. */
	async requireWatch(
		label: string,
		target: PublicKey,
		targetOwner: PublicKey = this.program.programId,
		blockOffset: number = BLOCK_OFFSET
	): Promise<void> {
		const { serving, impostors } = await watchesOnTarget(this.connection, {
			target,
			targetOwner,
			blockOffset,
			creators: this.watchCreators,
		});
		if (serving.length > 0) return;

		const found = impostors.map(describeImpostor).join('; ');
		this.failures.push(
			`${label} ${target.toBase58()} has no relay watch from ` +
				`${this.watchCreators.map((key) => key.toBase58()).join(', ')}` +
				(found ? `. It has only: ${found}` : '')
		);
	}

	private reportFor(name: string): TypeReport {
		if (!this.reports.has(name))
			this.reports.set(name, { count: 0, undecodable: [], undersized: [] });
		return this.reports.get(name)!;
	}
}

async function main() {
	const argv = process.argv.slice(2);
	const flag = (name: string, fallback: string) => {
		const i = argv.indexOf(name);
		return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
	};

	const connection = new Connection(
		flag('--url', 'http://127.0.0.1:8899'),
		'confirmed'
	);
	const authority = Keypair.fromSecretKey(
		Uint8Array.from(JSON.parse(fs.readFileSync(flag('--keypair', ''), 'utf-8')))
	);
	const provider = new AnchorProvider(
		connection,
		new Wallet(authority) as any,
		{ commitment: 'confirmed' }
	);
	const program = new Program(
		JSON.parse(fs.readFileSync('packages/sdk/src/idl/velocity.json', 'utf-8')),
		provider
	);
	const velocity = program.programId;
	const verifier = new Verifier(
		connection,
		program,
		authority,
		await getVelocityStateAccountPublicKey(velocity),
		parseWatchCreators(
			argv.includes('--watch-creators') ? flag('--watch-creators', '') : undefined,
			authority.publicKey
		)
	);

	const accounts = await connection.getProgramAccounts(velocity, {
		commitment: 'confirmed',
	});
	for (const { pubkey, account } of accounts) {
		await verifier.checkAccount(pubkey, account.data);
	}

	await verifier.requireExists(
		'relay scratch',
		getRelayScratchPublicKey(velocity)
	);
	await verifier.requireExists(
		'crank treasury',
		getCrankTreasuryPublicKey(velocity)
	);
	// Every relay executor names the protocol User as its filler or taker.
	const signer = getVelocitySignerPublicKey(velocity);
	await verifier.requireExists(
		'protocol user',
		getUserAccountPublicKeySync(velocity, signer, 0)
	);
	await verifier.requireExists(
		'protocol user stats',
		getUserStatsAccountPublicKey(velocity, signer)
	);

	const perpMarkets = accounts.filter(({ account }) =>
		nameIs(program, account.data, 'PerpMarket')
	);
	for (const { account } of perpMarkets) {
		const market: any = program.coder.accounts.decode(
			'perpMarket',
			account.data
		);
		await verifier.requireExists(
			`market ${market.marketIndex} quoter slab`,
			getQuoterSlabPublicKey(velocity, market.marketIndex)
		);
		if (market.clobMarket.equals(PublicKey.default)) {
			console.log(`note: perp market ${market.marketIndex} has no CLOB book`);
			continue;
		}

		await requireMarketWatches(verifier, program, market);
	}

	await requireWatchesOnExposedUsers(verifier, program, accounts);

	printReport(verifier, accounts.length);
	if (verifier.failures.length > 0) process.exit(1);
}

/** A market with a book needs a watch on its crank conditions, and one on the
 * book at the offset the attach recorded. */
async function requireMarketWatches(
	verifier: Verifier,
	program: Program,
	market: { marketIndex: number; clobMarket: PublicKey }
): Promise<void> {
	const conditions = getClobCrankConditionsPublicKey(
		program.programId,
		market.marketIndex
	);
	const info = await program.provider.connection.getAccountInfo(conditions);
	if (!info) {
		verifier.failures.push(
			`market ${market.marketIndex} names a book but has no crank conditions ${conditions.toBase58()}`
		);
		return;
	}

	await verifier.requireWatch(`market ${market.marketIndex} crank conditions`, conditions);
	const { clobBlockOffset } = program.coder.accounts.decode(
		'clobCrankConditionsV0',
		info.data
	) as { clobBlockOffset: number };
	const book = await program.provider.connection.getAccountInfo(market.clobMarket);
	if (book && clobBlockOffset) {
		await verifier.requireWatch(
			`market ${market.marketIndex} book`,
			market.clobMarket,
			book.owner,
			clobBlockOffset
		);
	}
}

/** migrate.ts covers a user with an open perp position, so only that user's conditions need a
 * watch. A user created after the upgrade has conditions before it trades. */
async function requireWatchesOnExposedUsers(
	verifier: Verifier,
	program: Program,
	accounts: readonly { pubkey: PublicKey; account: { data: Buffer } }[]
): Promise<void> {
	const users = new Map(
		accounts
			.filter(({ account }) => nameIs(program, account.data, 'User'))
			.map(({ pubkey, account }) => [
				pubkey.toBase58(),
				decodeUser(account.data),
			])
	);
	const conditions = accounts.filter(({ account }) =>
		nameIs(program, account.data, 'UserConditionsV0')
	);
	for (const { pubkey, account } of conditions) {
		const { user } = program.coder.accounts.decode(
			'userConditionsV0',
			account.data
		) as {
			user: PublicKey;
		};
		const exposed = users
			.get(user.toBase58())
			?.perpPositions.some((position) => !positionIsAvailable(position));
		if (exposed) await verifier.requireWatch('user conditions', pubkey);
	}
}

function nameIs(program: Program, data: Buffer, name: string): boolean {
	const account = (program.rawIdl as any).accounts.find(
		(a: any) => a.name === name
	);
	return Buffer.from(account.discriminator).equals(data.subarray(0, 8));
}

function printReport(verifier: Verifier, total: number): void {
	console.log(`\n${total} velocity accounts`);
	for (const [name, report] of [...verifier.reports].sort()) {
		const problems = [...report.undecodable, ...report.undersized];
		console.log(
			`  ${name.padEnd(28)} ${String(report.count).padStart(5)}  undecodable ${
				report.undecodable.length
			}  undersized ${report.undersized.length}`
		);
		problems.slice(0, 5).forEach((line) => console.log(`      ${line}`));
		verifier.failures.push(...problems.map((line) => `${name} ${line}`));
	}

	console.log(
		verifier.failures.length === 0
			? '\nverify: clean'
			: `\nverify: ${verifier.failures.length} failures`
	);
	verifier.failures.slice(0, 40).forEach((line) => console.log(`  ${line}`));
}

main().catch((error) => {
	console.error(error);
	process.exit(1);
});
