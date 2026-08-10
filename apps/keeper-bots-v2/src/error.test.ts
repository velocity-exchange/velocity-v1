import { expect } from 'chai';
import { getErrorCodeFromSimError } from './error';

describe('getErrorCodeFromSimError', () => {
	// A landed-Ok transaction has `meta.err === null`, and the confirm loop feeds
	// that straight in. Dereferencing it threw a TypeError into the confirmation
	// batch, which aborted the loop and stalled every pending signature behind it.
	it('returns null for a landed-Ok transaction (meta.err === null)', () => {
		expect(getErrorCodeFromSimError(null)).to.be.null;
	});

	it('returns null for a string error', () => {
		expect(getErrorCodeFromSimError('BlockhashNotFound')).to.be.null;
	});

	it('returns null for a non-instruction error', () => {
		expect(getErrorCodeFromSimError({ InsufficientFundsForFee: {} } as never))
			.to.be.null;
	});

	it('extracts the raw Custom code from an InstructionError', () => {
		expect(
			getErrorCodeFromSimError({
				InstructionError: [3, { Custom: 6239 }],
			} as never)
		).to.equal(6239);
	});

	it('returns null when the instruction error is not a Custom code', () => {
		expect(
			getErrorCodeFromSimError({
				InstructionError: [3, 'ComputationalBudgetExceeded'],
			} as never)
		).to.be.null;
	});
});
