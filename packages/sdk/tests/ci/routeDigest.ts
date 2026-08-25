/**
 * Parity tests for the signed-route digest. The byte cases are the ones the
 * program pins in `state::order_params::tests::a_signed_route_digests_canonically`,
 * so a divergence fails here rather than as a filler whose claimed route the
 * program rejects.
 */
import { PublicKey } from '@solana/web3.js';
import { assert } from 'chai';
import { getRouteDigest } from '../../src';

const a = new PublicKey(new Uint8Array(32).fill(7));
const b = new PublicKey(new Uint8Array(32).fill(9));

describe('signed route digest', () => {
	it('digests no route to zero', () => {
		assert.deepEqual(getRouteDigest(), [0, 0, 0, 0]);
		assert.deepEqual(getRouteDigest(null), [0, 0, 0, 0]);
		assert.deepEqual(getRouteDigest([]), [0, 0, 0, 0]);
	});

	it('matches the bytes the program pins', () => {
		assert.deepEqual(getRouteDigest([a]), [0x4b, 0xb0, 0x6f, 0x8e]);
		assert.deepEqual(getRouteDigest([a, b]), [0x49, 0x44, 0x0f, 0xa4]);
	});

	it('canonicalises the route before digesting it', () => {
		assert.deepEqual(getRouteDigest([a, b]), getRouteDigest([b, a]));
		assert.deepEqual(getRouteDigest([a, b, a]), getRouteDigest([a, b]));
	});

	it('keeps a real route distinguishable from an absent one', () => {
		assert.notDeepEqual(getRouteDigest([a]), getRouteDigest([a, b]));
		assert.notDeepEqual(getRouteDigest([a]), [0, 0, 0, 0]);
	});
});
