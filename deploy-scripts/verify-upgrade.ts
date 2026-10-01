/**
 * allow-verbose: the usage header of an operator script.
 *
 * Check that every velocity account is readable by the deployed program after an upgrade and
 * `migrate.ts`. Exits non-zero on any failure.
 *
 *   bun run deploy-scripts/verify-upgrade.ts --url <rpc> --keypair <payer> \
 *     [--extend-authority <pk>] [--watch-creators <pk,pk>] \
 *     [--clob-hash <sha256> | --clob-so <path>] [--expect-hot]
 *
 * A user's relay watch counts only when the keypair, or a key `--watch-creators` names, created it
 * at the block offset. Pass the migration payer there when it is not the keypair.
 *
 * The size check simulates `extend_account` on each account and fails when the simulation would
 * grow it. The target size comes from the deployed binary, so the check does not trust the size
 * table in `migrate.ts`. The simulation verifies no signature, so `--extend-authority` can name
 * the vault, and the check runs after the payer's hot roles are revoked.
 *
 * It also fails while the upgrade pause is set, while the fee rails, the treasury pricing or the
 * SOL spot market index are unset, while the migration hot roles are set, unless `--expect-hot`
 * makes those warnings, and when the CLOB code does not hash to `--clob-hash`.
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
	getUserConditionsPublicKey,
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
import {
	deployedProgramHash,
	expectedClobHash,
	UPGRADE_PAUSE_BITS,
} from './upgrade-guards';

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
	readonly warnings: string[] = [];
	private names = new Map<string, string>();
	private coder: BorshAccountsCoder;

	constructor(
		private connection: Connection,
		private program: Program,
		private payer: Keypair,
		private extendAuthority: PublicKey,
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
				payer: this.payer.publicKey,
				authority: this.extendAuthority,
				account,
				systemProgram: SystemProgram.programId,
			})
			.instruction();
		const message = new TransactionMessage({
			payerKey: this.payer.publicKey,
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
	const payer = Keypair.fromSecretKey(
		Uint8Array.from(JSON.parse(fs.readFileSync(flag('--keypair', ''), 'utf-8')))
	);
	const provider = new AnchorProvider(connection, new Wallet(payer) as any, {
		commitment: 'confirmed',
	});
	const program = new Program(
		JSON.parse(fs.readFileSync('packages/sdk/src/idl/velocity.json', 'utf-8')),
		provider
	);
	const velocity = program.programId;
	const verifier = new Verifier(
		connection,
		program,
		payer,
		new PublicKey(flag('--extend-authority', payer.publicKey.toBase58())),
		await getVelocityStateAccountPublicKey(velocity),
		parseWatchCreators(
			argv.includes('--watch-creators')
				? flag('--watch-creators', '')
				: undefined,
			payer.publicKey
		)
	);

	const accounts = await connection.getProgramAccounts(velocity, {
		commitment: 'confirmed',
	});
	for (const { pubkey, account } of accounts) {
		await verifier.checkAccount(pubkey, account.data);
	}

	await requireSingletons(verifier, velocity);

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
	await requireUpgradeFinished(verifier, program, accounts, {
		expectHot: argv.includes('--expect-hot'),
		clobHash: expectedClobHash(
			argv.includes('--clob-hash') ? flag('--clob-hash', '') : undefined,
			argv.includes('--clob-so') ? flag('--clob-so', '') : undefined
		),
	});

	printReport(verifier, accounts.length);
	if (verifier.failures.length > 0) process.exit(1);
}

/** The accounts every relay crank loads. Every relay executor names the
 * protocol User as its filler or taker. */
async function requireSingletons(
	verifier: Verifier,
	velocity: PublicKey
): Promise<void> {
	const signer = getVelocitySignerPublicKey(velocity);
	await verifier.requireExists(
		'relay scratch',
		getRelayScratchPublicKey(velocity)
	);
	await verifier.requireExists(
		'crank treasury',
		getCrankTreasuryPublicKey(velocity)
	);
	await verifier.requireExists(
		'protocol user',
		getUserAccountPublicKeySync(velocity, signer, 0)
	);
	await verifier.requireExists(
		'protocol user stats',
		getUserStatsAccountPublicKey(velocity, signer)
	);
}

/** The State settings and the CLOB code the upgrade leaves behind. */
async function requireUpgradeFinished(
	verifier: Verifier,
	program: Program,
	accounts: readonly {
		pubkey: PublicKey;
		account: { data: Buffer; lamports: number };
	}[],
	options: { expectHot: boolean; clobHash?: string }
): Promise<void> {
	const stateAccount = accounts.find(({ account }) =>
		nameIs(program, account.data, 'State')
	);
	if (!stateAccount) {
		verifier.failures.push('State does not exist');
		return;
	}

	const state: any = program.coder.accounts.decode(
		'state',
		stateAccount.account.data
	);
	requireMigrationHotRolesRevoked(verifier, state, options.expectHot);
	requireLiveExchange(verifier, state);
	requireEconomics(verifier, program, state, accounts);
	await requireClobCode(verifier, program, options.clobHash);
}

/** The migration gives the payer two hot roles. The conditionsSync key can set
 * or clear every user's paid resync terms, so a role left set is a failure.
 * `--expect-hot` makes it a warning for a run before the revoke executes. */
function requireMigrationHotRolesRevoked(
	verifier: Verifier,
	state: any,
	expectHot: boolean
): void {
	for (const [role, key] of [
		['conditionsSync', state.hotConditionsSync],
		['accountExtension', state.hotAccountExtension],
	] as [string, PublicKey][]) {
		if (key.equals(PublicKey.default)) continue;

		const line =
			`hot ${role} is still ${key.toBase58()}. Revoke it: velocity-admin --multisig <pda> ` +
			`auth set-hot-admin ${role} ${PublicKey.default.toBase58()}`;
		(expectHot ? verifier.warnings : verifier.failures).push(line);
	}
}

/** The upgrade pause must be lifted. Otherwise no liquidation, settle or
 * withdrawal runs, for as long as nobody notices. */
function requireLiveExchange(verifier: Verifier, state: any): void {
	const held = state.exchangeStatus & UPGRADE_PAUSE_BITS;
	if (held !== 0) {
		verifier.failures.push(
			`exchange status ${state.exchangeStatus} still holds upgrade pause bits ${held}. ` +
				'Execute the lift that migrate.ts --lift-upgrade-pause proposed.'
		);
	}
}

/** What the migration writes so relay is paid: non-zero fee rails, a priced
 * and funded crank treasury, and the SOL spot market for the reimbursement. */
function requireEconomics(
	verifier: Verifier,
	program: Program,
	state: any,
	accounts: readonly {
		pubkey: PublicKey;
		account: { data: Buffer; lamports: number };
	}[]
): void {
	const rails = state.transactionFeeRails;
	const railsPriced =
		rails.inclusionLamports > 0 ||
		rails.signatureLamports > 0 ||
		(rails.resourceFeeNumerator > 0 && rails.resourceFeeDenominator > 0);
	if (!railsPriced) {
		verifier.failures.push(
			'the transaction fee rails price every crank at zero'
		);
	}

	const treasuryKey = getCrankTreasuryPublicKey(program.programId);
	const treasury = accounts.find(({ pubkey }) => pubkey.equals(treasuryKey));
	if (treasury) {
		const decoded: any = program.coder.accounts.decode(
			'crankTreasuryV0',
			treasury.account.data
		);
		if (!decoded.refillTargetCranks || !decoded.refillWatermarkCranks) {
			verifier.failures.push(
				`crank treasury ${treasuryKey.toBase58()} is not priced`
			);
		}

		if (decoded.resyncFloorLamports.isZero()) {
			verifier.failures.push(
				`crank treasury ${treasuryKey.toBase58()} has no resync floor, so paid resyncs ` +
					'can spend what reservoir refills need'
			);
		}
	}

	const hasSolMarket = accounts
		.filter(({ account }) => nameIs(program, account.data, 'SpotMarket'))
		.map(({ account }) =>
			program.coder.accounts.decode('spotMarket', account.data)
		)
		.some(
			(market: any) =>
				market.marketIndex !== 0 && market.mint.equals(NATIVE_MINT)
		);
	if (state.solSpotMarketIndex === 0) {
		(hasSolMarket ? verifier.failures : verifier.warnings).push(
			'State.solSpotMarketIndex is 0, so the liquidation reimbursement and the SOL ' +
				'payment floors are off' +
				(hasSolMarket ? '' : '. No wrapped-SOL spot market exists to name')
		);
	}
}

/** The CLOB is the trust root for maker identity, so its code must be the
 * reviewed build. Without a hash this is a warning. */
async function requireClobCode(
	verifier: Verifier,
	program: Program,
	expectedHash: string | undefined
): Promise<void> {
	const attach = (program.rawIdl as any).instructions.find(
		(ix: any) => ix.name === 'update_perp_market_clob_quoter'
	);
	const address = attach?.accounts?.find(
		(account: any) => account.name === 'clob_program'
	)?.address;
	if (!address) {
		verifier.failures.push('the IDL does not pin a clob_program on the attach');
		return;
	}

	if (!expectedHash) {
		verifier.warnings.push(
			'no --clob-hash or --clob-so, so the CLOB code is not checked'
		);
		return;
	}

	const deployed = await deployedProgramHash(
		program.provider.connection,
		new PublicKey(address)
	);
	if (deployed !== expectedHash) {
		verifier.failures.push(
			`the CLOB ${address} hashes to ${
				deployed ?? 'nothing'
			}, not ${expectedHash}`
		);
	}
}

const NATIVE_MINT = new PublicKey(
	'So11111111111111111111111111111111111111112'
);

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
			`market ${
				market.marketIndex
			} names a book but has no crank conditions ${conditions.toBase58()}`
		);
		return;
	}

	await verifier.requireWatch(
		`market ${market.marketIndex} crank conditions`,
		conditions
	);
	const { clobBlockOffset } = program.coder.accounts.decode(
		'clobCrankConditionsV0',
		info.data
	) as { clobBlockOffset: number };
	const book = await program.provider.connection.getAccountInfo(
		market.clobMarket
	);
	if (book && clobBlockOffset) {
		await verifier.requireWatch(
			`market ${market.marketIndex} book`,
			market.clobMarket,
			book.owner,
			clobBlockOffset
		);
	}
}

/** migrate.ts covers a user with an open perp position, so every such user
 * needs conditions and a watch on them. A user created after the upgrade has
 * conditions before it trades. */
async function requireWatchesOnExposedUsers(
	verifier: Verifier,
	program: Program,
	accounts: readonly { pubkey: PublicKey; account: { data: Buffer } }[]
): Promise<void> {
	const conditionsKeys = new Set(
		accounts
			.filter(({ account }) =>
				nameIs(program, account.data, 'UserConditionsV0')
			)
			.map(({ pubkey }) => pubkey.toBase58())
	);
	const exposedUsers = accounts
		.filter(({ account }) => nameIs(program, account.data, 'User'))
		.filter(({ account }) =>
			decodeUser(account.data).perpPositions.some(
				(position) => !positionIsAvailable(position)
			)
		);
	for (const { pubkey: user } of exposedUsers) {
		const conditions = getUserConditionsPublicKey(program.programId, user);
		if (!conditionsKeys.has(conditions.toBase58())) {
			verifier.failures.push(
				`user ${user.toBase58()} holds a perp position and has no conditions ${conditions.toBase58()}`
			);
			continue;
		}

		await verifier.requireWatch(
			`user ${user.toBase58()} conditions`,
			conditions
		);
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

	verifier.warnings.forEach((line) => console.log(`warning: ${line}`));
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
