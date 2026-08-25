/**
 * Parity tests for the crank-fee mirror. Every case here is transcribed from
 * the program's own tests in `state/clob_crank.rs`, so a divergence between
 * `deriveCrankPayments` and `CrankPaymentsV0::derive` fails here rather than
 * surfacing as a client that mispredicts what an attach will write.
 */
import { assert } from 'chai';
import {
	CrankCostUnitsV0,
	deriveCrankPayments,
	INSTRUCTION_DATA_BYTES_PER_COST_UNIT,
	LOADED_ACCOUNTS_PAGE_BYTES,
	LOADED_ACCOUNTS_PAGE_COST_UNITS,
	requestedCostUnits,
	SIGNATURE_COST_UNITS,
	setLoadedAccountsDataSizeLimitIx,
	transactionCost,
	TransactionFeeRails,
	WRITE_LOCK_COST_UNITS,
} from '../../src';

/** The fee model that charges for signatures and nothing else. */
const FLAT_PER_SIGNATURE: TransactionFeeRails = {
	inclusionLamports: 0,
	signatureLamports: 5_000,
	resourceFeeNumerator: 0,
	resourceFeeDenominator: 0,
};

const UNITS: CrankCostUnitsV0 = {
	removal: 30_000,
	cross: 180_000,
	takerOriginCross: 190_000,
	trigger: 40_000,
	liquidation: 120_000,
	forceCancel: 60_000,
	refill: 30_000,
};

describe('crank fee mirror', () => {
	it('prices every crank the same under a flat per-signature fee', () => {
		const flat = deriveCrankPayments(FLAT_PER_SIGNATURE, UNITS);
		assert.equal(flat.removal, 5_000);
		assert.equal(flat.cross, 5_000);
		assert.equal(flat.takerOriginCross, 5_000);
	});

	it('separates them once cost units are charged for', () => {
		const rails: TransactionFeeRails = {
			inclusionLamports: 2_500,
			signatureLamports: 0,
			resourceFeeNumerator: 1,
			resourceFeeDenominator: 2,
		};
		const priced = deriveCrankPayments(rails, UNITS);
		assert.equal(priced.removal, 2_500 + 15_000);
		assert.equal(priced.cross, 2_500 + 90_000);
		assert.equal(priced.takerOriginCross, 2_500 + 95_000);
		assert.equal(priced.trigger, 2_500 + 20_000);
		assert.equal(priced.liquidation, 2_500 + 60_000);
		assert.equal(priced.forceCancel, 2_500 + 30_000);
		// A removal paid the cross's price is four times what it costs; a cross
		// paid the removal's price is a crank nobody runs.
		assert.isAbove(priced.cross, priced.removal * 4);
	});

	it('rounds the rate up and treats a zero denominator as no resource fee', () => {
		const tenth: TransactionFeeRails = {
			inclusionLamports: 2_500,
			signatureLamports: 0,
			resourceFeeNumerator: 1,
			resourceFeeDenominator: 10,
		};
		// A third of a lamport still costs one: a payment short by a lamport
		// buys nothing.
		assert.equal(transactionCost(tenth, 3, 1), 2_501);
		assert.equal(transactionCost(tenth, 30_000, 1), 2_500 + 3_000);

		const off: TransactionFeeRails = {
			inclusionLamports: 2_500,
			signatureLamports: 5_000,
			resourceFeeNumerator: 1,
			resourceFeeDenominator: 0,
		};
		assert.equal(transactionCost(off, 1_000_000, 2), 2_500 + 10_000);
	});

	it('sums the cost model the way the runtime does', () => {
		const units = requestedCostUnits({
			signatures: 1,
			writeLocks: 12,
			instructionDataBytes: 280,
			requestedComputeUnits: 260_000,
			requestedLoadedAccountsDataSize: 8 * 1024 * 1024,
		});
		const pages = (8 * 1024 * 1024) / LOADED_ACCOUNTS_PAGE_BYTES;
		assert.equal(
			units,
			SIGNATURE_COST_UNITS +
				12 * WRITE_LOCK_COST_UNITS +
				Math.floor(280 / INSTRUCTION_DATA_BYTES_PER_COST_UNIT) +
				260_000 +
				pages * LOADED_ACCOUNTS_PAGE_COST_UNITS
		);

		// The default nobody asks for: 64 MiB is 16,384 cost units, which on
		// its own outweighs the whole of a light transaction.
		const unaskedFor = requestedCostUnits({
			signatures: 0,
			writeLocks: 0,
			instructionDataBytes: 0,
			requestedComputeUnits: 0,
			requestedLoadedAccountsDataSize: 64 * 1024 * 1024,
		});
		assert.equal(unaskedFor, 16_384);
	});

	it('encodes the loaded-accounts limit the compute budget program reads', () => {
		const ix = setLoadedAccountsDataSizeLimitIx(12 * 1024 * 1024);
		assert.equal(ix.keys.length, 0);
		assert.equal(ix.data.length, 5);
		assert.equal(ix.data[0], 4);
		assert.equal(ix.data.readUInt32LE(1), 12 * 1024 * 1024);
	});
});
