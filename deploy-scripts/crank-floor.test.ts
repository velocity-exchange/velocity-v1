// bun test deploy-scripts/crank-floor.test.ts
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
	CROSS_FALLBACK_MIN_PAYMENT_OFFSET,
	crossFallbackFloor,
	crossFallbackFloorIsStale,
} from './crank-floor';

const payments = { removal: 3_000, cross: 9_000, takerOriginCross: 8_000 };

function conditionsWithFloor(floor: bigint): Buffer {
	const data = Buffer.alloc(CROSS_FALLBACK_MIN_PAYMENT_OFFSET + 8);
	data.writeBigUInt64LE(floor, CROSS_FALLBACK_MIN_PAYMENT_OFFSET);
	return data;
}

test('the fallback floor is the lowest of the crosses and the removal', () => {
	assert.equal(crossFallbackFloor(payments), 3_000n);
	assert.equal(crossFallbackFloor({ ...payments, cross: 2_000 }), 2_000n);
});

test('a floor written before the band cancel is stale', () => {
	assert.equal(
		crossFallbackFloorIsStale(conditionsWithFloor(8_000n), payments),
		true
	);
	assert.equal(
		crossFallbackFloorIsStale(conditionsWithFloor(3_000n), payments),
		false
	);
});
