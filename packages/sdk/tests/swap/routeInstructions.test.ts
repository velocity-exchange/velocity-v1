import { expect } from 'chai';
import { PublicKey, TransactionInstruction } from '@solana/web3.js';
import { filterRouteInstructions } from '../../src/swap/routeInstructions';

const INPUT_MINT = new PublicKey('So11111111111111111111111111111111111111112');
const OUTPUT_MINT = new PublicKey(
	'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'
);
const OTHER_MINT = new PublicKey('mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So');
const PAYER = new PublicKey('HxFLKUAmAMLz1jtT3hbvCMELwH5H9tpM2QugP8sKyfhc');

const COMPUTE_BUDGET = new PublicKey(
	'ComputeBudget111111111111111111111111111111'
);
const TOKEN_PROGRAM = new PublicKey(
	'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
);
const SYSTEM_PROGRAM = new PublicKey('11111111111111111111111111111111');
const ATA_PROGRAM = new PublicKey(
	'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'
);
const AMM_PROGRAM = new PublicKey(
	'675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8'
);

const ix = (programId: PublicKey, keys: PublicKey[] = []) =>
	new TransactionInstruction({
		programId,
		keys: keys.map((pubkey) => ({ pubkey, isSigner: false, isWritable: true })),
		data: Buffer.from([]),
	});

/** ATA creation instruction: the mint sits at key index 3. */
const ataIx = (mint: PublicKey) => ix(ATA_PROGRAM, [PAYER, PAYER, PAYER, mint]);

const filter = (instructions: TransactionInstruction[]) =>
	filterRouteInstructions({
		instructions,
		inputMint: INPUT_MINT,
		outputMint: OUTPUT_MINT,
	});

describe('filterRouteInstructions', () => {
	it('drops the setup velocity supplies itself', () => {
		const kept = filter([
			ix(COMPUTE_BUDGET),
			ix(TOKEN_PROGRAM),
			ix(SYSTEM_PROGRAM),
			ix(AMM_PROGRAM),
		]);

		expect(kept).to.have.lengthOf(1);
		expect(kept[0].programId.equals(AMM_PROGRAM)).to.be.true;
	});

	it('drops ATA creation for the input and output mints', () => {
		expect(filter([ataIx(INPUT_MINT)])).to.be.empty;
		expect(filter([ataIx(OUTPUT_MINT)])).to.be.empty;
	});

	it('keeps ATA creation for any other mint', () => {
		// An intermediate hop's account is created by nobody else, so dropping
		// it would leave the route referencing an account that never exists.
		expect(filter([ataIx(OTHER_MINT)])).to.have.lengthOf(1);
	});

	it('keeps a short ATA instruction instead of throwing on it', () => {
		// Previously indexed keys[3] unguarded, which threw on a malformed
		// instruction rather than leaving it alone.
		const short = ix(ATA_PROGRAM, [PAYER, PAYER]);

		expect(() => filter([short])).to.not.throw();
		expect(filter([short])).to.have.lengthOf(1);
	});
});
