import { Connection, PublicKey } from '@solana/web3.js';
import { OracleClient, OraclePriceData } from './types';

export class SwitchboardClient implements OracleClient {
	connection: Connection;

	public constructor(connection: Connection) {
		this.connection = connection;
	}

	public async getOraclePriceData(
		_pricePublicKey: PublicKey
	): Promise<OraclePriceData> {
		throw new Error('Switchboard oracle support has been removed');
	}

	public getOraclePriceDataFromBuffer(_buffer: Buffer): OraclePriceData {
		throw new Error('Switchboard oracle support has been removed');
	}
}
