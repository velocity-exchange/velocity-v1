/**
 * allow-verbose: the usage header of an operator script.
 *
 * Check that every velocity account is readable by the deployed program after an upgrade and
 * `migrate.ts`. Exits non-zero on any failure.
 *
 *   bun run deploy-scripts/verify-upgrade.ts --url <rpc> --keypair <extension authority>
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
	getCrankTreasuryPublicKey,
	getQuoterSlabPublicKey,
	getRelayScratchPublicKey,
	getVelocityStateAccountPublicKey,
	positionIsAvailable,
	Wallet,
} from '@velocity-exchange/sdk';

const RELAY_PROGRAM = new PublicKey(
	process.env.RELAY_PROGRAM_ID ?? '4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu'
);
/** `WatchV0` holds `target_program`, then `target`. */
const WATCH_TARGET_OFFSET = 40;
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
		private state: PublicKey
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

	async requireWatch(label: string, target: PublicKey): Promise<void> {
		const watches = await this.connection.getProgramAccounts(RELAY_PROGRAM, {
			filters: [
				{ memcmp: { offset: WATCH_TARGET_OFFSET, bytes: target.toBase58() } },
			],
			dataSlice: { offset: 0, length: 0 },
		});

		if (watches.length === 0)
			this.failures.push(`${label} ${target.toBase58()} has no relay watch`);
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
		await getVelocityStateAccountPublicKey(velocity)
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
		}
	}

	// migrate.ts covers a user with an open perp position, so only that user's
	// conditions need a watch. A new user's conditions exist before it trades.
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

	printReport(verifier, accounts.length);
	if (verifier.failures.length > 0) process.exit(1);
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
