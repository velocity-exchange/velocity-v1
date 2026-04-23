import { Connection, PublicKey } from '@solana/web3.js';

export type FetchAccountOptions = {
	commitment?: Parameters<Connection['getAccountInfo']>[1];
};

export async function fetchAccount(
	connection: Connection,
	publicKey: PublicKey,
	opts?: FetchAccountOptions
): Promise<Buffer | null> {
	const info = await connection.getAccountInfo(publicKey, opts?.commitment);
	return info?.data ?? null;
}

export async function fetchAccounts(
	connection: Connection,
	publicKeys: PublicKey[],
	opts?: FetchAccountOptions
): Promise<(Buffer | null)[]> {
	const infos = await connection.getMultipleAccountsInfo(
		publicKeys,
		opts?.commitment
	);
	return infos.map((info) => info?.data ?? null);
}
