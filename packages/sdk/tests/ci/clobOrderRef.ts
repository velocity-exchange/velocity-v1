import { expect } from 'chai';
import { decodeClobOrderRefV0 } from '../../src/clob/orderRef';

describe('decodeClobOrderRefV0', () => {
	it('reads the node index and the book order id as the program writes them', () => {
		const data = Buffer.alloc(12);
		data.writeUInt32LE(7, 0);
		data.writeBigUInt64LE(BigInt('18446744073709551000'), 4);

		const orderRef = decodeClobOrderRefV0(data);

		expect(orderRef?.nodeIndex).to.equal(7);
		expect(orderRef?.orderId.toString()).to.equal('18446744073709551000');
	});

	it('reads nothing from return data of another length', () => {
		expect(decodeClobOrderRefV0(new Uint8Array(8))).to.equal(undefined);
	});
});
