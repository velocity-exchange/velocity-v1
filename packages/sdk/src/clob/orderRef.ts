import { BN } from '@coral-xyz/anchor';
import { ClobOrderRefV0 } from '../types';

/** A `ClobOrderRefV0` on the wire: a little-endian `u32` node index, then a `u64` order id. */
export const CLOB_ORDER_REF_V0_LEN = 12;

/**
 * The handle `placeAndMakePerpOrderV1` writes as return data. `undefined` when the
 * transaction's last return data is not the placement's.
 */
export function decodeClobOrderRefV0(
	returnData: Uint8Array
): ClobOrderRefV0 | undefined {
	if (returnData.length !== CLOB_ORDER_REF_V0_LEN) return undefined;

	const bytes = Buffer.from(returnData);
	return {
		nodeIndex: bytes.readUInt32LE(0),
		orderId: new BN(bytes.subarray(4, 12), 'le'),
	};
}
