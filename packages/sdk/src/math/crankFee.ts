import {
	CrankCostUnitsV0,
	CrankPaymentsV0,
	TransactionFeeRails,
} from '../types';

/**
 * The block-packing cost model, mirrored from the runtime.
 *
 * A transaction's *requested cost units* are what the network prices it by:
 * the saturating sum of its signatures, its write locks, its instruction-data
 * bytes, the compute limit it requests, and the loaded-accounts data size it
 * requests. The last two are the requested figures, not the consumed ones — a
 * transaction pays for the room it asks for.
 *
 * These constants are the runtime's, so a change there is a change here. They
 * are exported because sizing a crank payment starts by measuring the crank,
 * and a measurement needs the same arithmetic the network does.
 */
export const SIGNATURE_COST_UNITS = 720;
export const WRITE_LOCK_COST_UNITS = 300;
export const INSTRUCTION_DATA_BYTES_PER_COST_UNIT = 140;
export const LOADED_ACCOUNTS_PAGE_BYTES = 32 * 1024;
export const LOADED_ACCOUNTS_PAGE_COST_UNITS = 8;

/** The shape of a transaction, as far as the cost model is concerned. */
export type TransactionShape = {
	signatures: number;
	writeLocks: number;
	instructionDataBytes: number;
	/** the compute limit the transaction requests */
	requestedComputeUnits: number;
	/** the loaded-accounts data size the transaction requests, in bytes */
	requestedLoadedAccountsDataSize: number;
};

/**
 * Cost units a transaction of this shape requests.
 *
 * @param shape - what the transaction carries and asks for.
 * @returns the cost-unit total the network prices it by.
 */
export function requestedCostUnits(shape: TransactionShape): number {
	const loadedPages = Math.ceil(
		shape.requestedLoadedAccountsDataSize / LOADED_ACCOUNTS_PAGE_BYTES
	);
	return (
		shape.signatures * SIGNATURE_COST_UNITS +
		shape.writeLocks * WRITE_LOCK_COST_UNITS +
		Math.floor(
			shape.instructionDataBytes / INSTRUCTION_DATA_BYTES_PER_COST_UNIT
		) +
		shape.requestedComputeUnits +
		loadedPages * LOADED_ACCOUNTS_PAGE_COST_UNITS
	);
}

/**
 * Lamports a transaction costs whoever sends it.
 *
 * Mirrors `TransactionFeeRails::transaction_cost`. Rounded up, because this
 * sizes a payment and a payment short by a lamport buys nothing.
 *
 * @param rails - the fee model, read off `StateAccount.transactionFeeRails`.
 * @param costUnits - what the transaction requests, see `requestedCostUnits`.
 * @param signatures - how many signatures it carries.
 * @returns the cost in lamports.
 */
export function transactionCost(
	rails: TransactionFeeRails,
	costUnits: number,
	signatures: number
): number {
	const fixed = rails.inclusionLamports + rails.signatureLamports * signatures;
	if (rails.resourceFeeDenominator === 0) {
		return fixed;
	}
	return (
		fixed +
		Math.ceil(
			(costUnits * rails.resourceFeeNumerator) / rails.resourceFeeDenominator
		)
	);
}

/**
 * Price every one of a market's cranks off one measurement each.
 *
 * Mirrors `CrankPaymentsV0::derive`, which is what
 * `updatePerpMarketClobQuoter` runs on chain — so this predicts the payments a
 * given attach will write. A crank transaction carries exactly one signature,
 * the turner's fee payer; executors name no signer at all.
 *
 * @param rails - the fee model, read off `StateAccount.transactionFeeRails`.
 * @param units - cost units each crank requests, measured by simulating it.
 * @returns the lamport payment for each crank.
 */
export function deriveCrankPayments(
	rails: TransactionFeeRails,
	units: CrankCostUnitsV0
): CrankPaymentsV0 {
	const price = (costUnits: number) => transactionCost(rails, costUnits, 1);
	return {
		removal: price(units.removal),
		cross: price(units.cross),
		takerOriginCross: price(units.takerOriginCross),
		trigger: price(units.trigger),
		liquidation: price(units.liquidation),
		forceCancel: price(units.forceCancel),
	};
}
