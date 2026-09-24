/**
 * Parity test: signed-route digest must match the program's implementation.
 */
import { PublicKey } from '@solana/web3.js';
import { assert } from 'chai';
import { getRouteDigest, ROUTE_DIGEST_LEN } from '../../src';

const a = new PublicKey(new Uint8Array(32).fill(7));
const b = new PublicKey(new Uint8Array(32).fill(9));
const noRoute = new Array(ROUTE_DIGEST_LEN).fill(0);

describe('signed route digest', () => {
	it('digests no route to zero', () => {
		assert.deepEqual(getRouteDigest(), noRoute);
		assert.deepEqual(getRouteDigest(null), noRoute);
		assert.deepEqual(getRouteDigest([]), noRoute);
	});

	it('matches the bytes the program pins', () => {
		assert.deepEqual(
			getRouteDigest([a]),
			[0x4b, 0xb0, 0x6f, 0x8e, 0x4e, 0x3a, 0x77, 0x15]
		);
		assert.deepEqual(
			getRouteDigest([a, b]),
			[0x49, 0x44, 0x0f, 0xa4, 0x64, 0xb1, 0x71, 0xc0]
		);
	});

	it('canonicalises the route before digesting it', () => {
		assert.deepEqual(getRouteDigest([a, b]), getRouteDigest([b, a]));
		assert.deepEqual(getRouteDigest([a, b, a]), getRouteDigest([a, b]));
	});

	it('keeps a real route distinguishable from an absent one', () => {
		assert.notDeepEqual(getRouteDigest([a]), getRouteDigest([a, b]));
		assert.notDeepEqual(getRouteDigest([a]), noRoute);
	});
});
