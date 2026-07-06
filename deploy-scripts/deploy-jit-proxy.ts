/**
 * Finalizes the initial jit-proxy deploy to its create-with-seed vanity
 * address. Invoked by deploy-jit-proxy.sh after `solana program write-buffer`.
 *
 * The program id J1TPRoX… = sha256(base ‖ seed ‖ loader) has no keypair, so
 * the standard CLI initial-deploy path (which requires the program account to
 * sign its own creation) cannot be used. The upgradeable loader itself never
 * requires the program account to sign — only its creation does — so this
 * script sends one transaction containing:
 *   1. SystemProgram.createAccountWithSeed (signed by the base keypair) to
 *      create the 36-byte program account, skipped if it already exists
 *   2. the loader's DeployWithMaxDataLen instruction, which creates the
 *      programdata PDA, copies the buffer into it, and marks the program
 *      executable with DEPLOYER_KEYPAIR as upgrade authority
 *
 * Required env: CLUSTER, RPC_URL, DEPLOYER_KEYPAIR, BUFFER_PUBKEY
 * Optional env: BASE_KEYPAIR (required only while the program account doesn't
 *               exist), MAX_DATA_LEN, NON_INTERACTIVE=1 / YES=1
 */

import readline from 'readline';
import {
	Connection,
	Keypair,
	PublicKey,
	SystemProgram,
	SYSVAR_CLOCK_PUBKEY,
	SYSVAR_RENT_PUBKEY,
	Transaction,
	TransactionInstruction,
	sendAndConfirmTransaction,
} from '@solana/web3.js';
import { loadKeypair } from '../packages/sdk/src';

const JIT_PROXY_PROGRAM_ID = new PublicKey(
	'J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ'
);
// Ground with cavemanloverboy/vanity: address = sha256(base ‖ seed ‖ loader)
const BASE_PUBKEY = new PublicKey(
	'Fqo8WncpiP55ExPhuWLzMbm1cuYuJpXjhQPnd8ak3Pyn'
);
const SEED = 'VQz6PWOm0uMWZ41R';
const BPF_UPGRADEABLE_LOADER = new PublicKey(
	'BPFLoaderUpgradeab1e11111111111111111111111'
);

// UpgradeableLoaderState account discriminants (bincode u32 LE at offset 0)
const STATE_UNINITIALIZED = 0;
const STATE_BUFFER = 1;
const STATE_PROGRAM = 2;
// Buffer layout: u32 discriminant, Option<Pubkey> authority (1 + 32), then ELF
const BUFFER_METADATA_LEN = 37;
const PROGRAM_ACCOUNT_LEN = 36; // u32 discriminant + programdata pubkey

function requireEnv(name: string): string {
	const v = process.env[name];
	if (!v) throw new Error(`${name} must be set`);
	return v;
}

async function confirm(prompt: string): Promise<void> {
	if (process.env.NON_INTERACTIVE === '1' || process.env.YES === '1') {
		console.log('(NON_INTERACTIVE/YES set — proceeding without prompt)');
		return;
	}
	const rl = readline.createInterface({
		input: process.stdin,
		output: process.stdout,
	});
	const answer = await new Promise<string>((resolve) =>
		rl.question(`${prompt} [y/N] `, resolve)
	);
	rl.close();
	if (!['y', 'Y', 'yes', 'YES'].includes(answer.trim())) {
		console.error('aborted.');
		process.exit(1);
	}
}

async function main() {
	const cluster = requireEnv('CLUSTER');
	const connection = new Connection(requireEnv('RPC_URL'), 'confirmed');
	const deployer: Keypair = loadKeypair(requireEnv('DEPLOYER_KEYPAIR'));
	const bufferPubkey = new PublicKey(requireEnv('BUFFER_PUBKEY'));

	const derived = await PublicKey.createWithSeed(
		BASE_PUBKEY,
		SEED,
		BPF_UPGRADEABLE_LOADER
	);
	if (!derived.equals(JIT_PROXY_PROGRAM_ID)) {
		throw new Error(
			`create-with-seed derivation mismatch: got ${derived.toBase58()}, expected ${JIT_PROXY_PROGRAM_ID.toBase58()}`
		);
	}

	const [programDataAddress] = PublicKey.findProgramAddressSync(
		[JIT_PROXY_PROGRAM_ID.toBuffer()],
		BPF_UPGRADEABLE_LOADER
	);

	const programAccount = await connection.getAccountInfo(JIT_PROXY_PROGRAM_ID);
	if (programAccount) {
		if (!programAccount.owner.equals(BPF_UPGRADEABLE_LOADER)) {
			throw new Error(
				`program account exists but is owned by ${programAccount.owner.toBase58()}, not the upgradeable loader`
			);
		}
		const state = programAccount.data.readUInt32LE(0);
		if (state === STATE_PROGRAM) {
			console.log(
				`${JIT_PROXY_PROGRAM_ID.toBase58()} is already deployed on ${cluster} ` +
					`(programdata ${programDataAddress.toBase58()}) — use a normal buffer upgrade:\n` +
					`  solana program deploy --program-id ${JIT_PROXY_PROGRAM_ID.toBase58()} --buffer <buffer> …`
			);
			return;
		}
		if (state !== STATE_UNINITIALIZED) {
			throw new Error(`program account is in unexpected loader state ${state}`);
		}
	}

	const bufferAccount = await connection.getAccountInfo(bufferPubkey);
	if (!bufferAccount || !bufferAccount.owner.equals(BPF_UPGRADEABLE_LOADER)) {
		throw new Error(
			`buffer ${bufferPubkey.toBase58()} not found or not loader-owned`
		);
	}
	if (bufferAccount.data.readUInt32LE(0) !== STATE_BUFFER) {
		throw new Error(
			`buffer ${bufferPubkey.toBase58()} is not in Buffer state (already consumed?)`
		);
	}
	const bufferAuthority = new PublicKey(bufferAccount.data.subarray(5, 37));
	if (
		bufferAccount.data[4] !== 1 ||
		!bufferAuthority.equals(deployer.publicKey)
	) {
		throw new Error(
			`buffer authority ${bufferAuthority.toBase58()} != deployer ${deployer.publicKey.toBase58()} — the loader requires them to match`
		);
	}
	const elfLen = bufferAccount.data.length - BUFFER_METADATA_LEN;
	const maxDataLen = process.env.MAX_DATA_LEN
		? parseInt(process.env.MAX_DATA_LEN, 10)
		: elfLen;
	if (!Number.isInteger(maxDataLen) || maxDataLen < elfLen) {
		throw new Error(`MAX_DATA_LEN ${maxDataLen} < program size ${elfLen}`);
	}

	const instructions: TransactionInstruction[] = [];
	const signers: Keypair[] = [deployer];

	if (!programAccount) {
		const baseKeypairPath = process.env.BASE_KEYPAIR;
		if (!baseKeypairPath) {
			throw new Error(
				`program account does not exist on ${cluster}; set BASE_KEYPAIR to the ` +
					`keypair for base pubkey ${BASE_PUBKEY.toBase58()} so it can be created with seed`
			);
		}
		const base = loadKeypair(baseKeypairPath);
		if (!base.publicKey.equals(BASE_PUBKEY)) {
			throw new Error(
				`BASE_KEYPAIR pubkey ${base.publicKey.toBase58()} != expected base ${BASE_PUBKEY.toBase58()}`
			);
		}
		instructions.push(
			SystemProgram.createAccountWithSeed({
				fromPubkey: deployer.publicKey,
				newAccountPubkey: JIT_PROXY_PROGRAM_ID,
				basePubkey: BASE_PUBKEY,
				seed: SEED,
				lamports: await connection.getMinimumBalanceForRentExemption(
					PROGRAM_ACCOUNT_LEN
				),
				space: PROGRAM_ACCOUNT_LEN,
				programId: BPF_UPGRADEABLE_LOADER,
			})
		);
		signers.push(base);
	}

	// UpgradeableLoaderInstruction::DeployWithMaxDataLen { max_data_len }
	// (bincode: u32 LE variant 2, u64 LE max_data_len)
	const deployData = Buffer.alloc(12);
	deployData.writeUInt32LE(2, 0);
	deployData.writeBigUInt64LE(BigInt(maxDataLen), 4);
	instructions.push(
		new TransactionInstruction({
			programId: BPF_UPGRADEABLE_LOADER,
			keys: [
				{ pubkey: deployer.publicKey, isSigner: true, isWritable: true },
				{ pubkey: programDataAddress, isSigner: false, isWritable: true },
				{ pubkey: JIT_PROXY_PROGRAM_ID, isSigner: false, isWritable: true },
				{ pubkey: bufferPubkey, isSigner: false, isWritable: true },
				{ pubkey: SYSVAR_RENT_PUBKEY, isSigner: false, isWritable: false },
				{ pubkey: SYSVAR_CLOCK_PUBKEY, isSigner: false, isWritable: false },
				{ pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
				{ pubkey: deployer.publicKey, isSigner: true, isWritable: false },
			],
			data: deployData,
		})
	);

	const programDataRent = await connection.getMinimumBalanceForRentExemption(
		45 + maxDataLen // UpgradeableLoaderState::size_of_programdata(max_data_len)
	);
	console.log('');
	console.log(`=== ${cluster} jit-proxy initial deploy ===`);
	console.log(`  program id        : ${JIT_PROXY_PROGRAM_ID.toBase58()}`);
	console.log(`  programdata       : ${programDataAddress.toBase58()}`);
	console.log(
		`  buffer            : ${bufferPubkey.toBase58()} (${elfLen} bytes)`
	);
	console.log(`  max_data_len      : ${maxDataLen}`);
	console.log(`  payer + authority : ${deployer.publicKey.toBase58()}`);
	console.log(
		`  create program acct: ${
			programAccount ? 'no (already exists)' : 'yes (with seed, base signs)'
		}`
	);
	console.log(
		`  programdata rent  : ${(programDataRent / 1e9).toFixed(
			3
		)} SOL (buffer rent is refunded to payer)`
	);
	console.log('');
	await confirm('Send deploy transaction?');

	const signature = await sendAndConfirmTransaction(
		connection,
		new Transaction().add(...instructions),
		signers,
		{ commitment: 'confirmed' }
	);
	console.log(`deploy tx: ${signature}`);

	const deployed = await connection.getAccountInfo(JIT_PROXY_PROGRAM_ID);
	if (!deployed?.executable) {
		throw new Error('program account is not executable after deploy');
	}
	console.log(
		`${JIT_PROXY_PROGRAM_ID.toBase58()} deployed on ${cluster}; upgrade authority ${deployer.publicKey.toBase58()}`
	);
}

main().then(
	() => process.exit(0),
	(err) => {
		console.error(err);
		process.exit(1);
	}
);
