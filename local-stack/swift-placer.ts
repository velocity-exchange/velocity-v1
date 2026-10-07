/**
 * The stack's swift keeper, in the shape of keep-rs `try_swift_place`. keep-rs needs gRPC.
 */
import { BN } from '@coral-xyz/anchor';
import {
	AccountMeta,
	ComputeBudgetProgram,
	Connection,
	Keypair,
	LAMPORTS_PER_SOL,
	PublicKey,
	TransactionInstruction,
	TransactionMessage,
} from '@solana/web3.js';
import {
	decodeQuoterSlab,
	DevnetPerpMarkets,
	FlowAttestationV0,
	getQuoterSlabPublicKey,
	getUserAccountPublicKeySync,
	getUserStatsAccountPublicKey,
	MakerInfo,
	SignedMsgOrderParamsDelegateMessage,
	SignedMsgOrderParamsMessage,
	UserAccount,
	VelocityClient,
} from '@velocity-exchange/sdk';
import bs58 from 'bs58';
import nacl from 'tweetnacl';
import {
	connectClient,
	DLOB_URL,
	fundWallet,
	loadKey,
	parseDecimal,
	RPC_URL,
	userAccountPublicKey,
} from './tools';

const SWIFT_URL = process.env.SWIFT_URL ?? 'http://swift:3003';
const SWIFT_WS_URL = process.env.SWIFT_WS_URL ?? 'ws://swift-ws:3004';
const KEY_PATH = '/state/keys/swift-placer.json';
const FILLER_DEPOSIT_DUSDT = '100';
const MAKERS_PER_PLACEMENT = 4;
const ATTEST_ATTEMPTS = 8;
const RECONNECT_MS = 2_000;

/** One order as swift's websocket sends it. */
type SwiftOrder = {
	uuid: string;
	order_message: string;
	order_signature: string;
	taker_authority: string;
	signing_authority: string;
};

/** Funds the placer and opens its account. The program credits every placement to a filler. */
async function openPlacer(connection: Connection): Promise<VelocityClient> {
	const signer = loadKey(KEY_PATH);
	const client = await connectClient(connection, signer);
	const user = userAccountPublicKey(client, signer.publicKey);

	if (!(await connection.getAccountInfo(user))) {
		console.log(`opening placer ${signer.publicKey.toBase58()}`);
		const tokenAccount = await fundWallet(
			connection,
			signer.publicKey,
			100,
			FILLER_DEPOSIT_DUSDT
		);
		await client.initializeUserAccountAndDepositCollateral(
			parseDecimal(FILLER_DEPOSIT_DUSDT, 6),
			tokenAccount
		);
	} else if (
		(await connection.getBalance(signer.publicKey)) < LAMPORTS_PER_SOL
	) {
		await fundWallet(connection, signer.publicKey, 100, '0');
	}

	await client.addUser(0);
	return client;
}

/** The book's best resting owners on the side the taker takes, without the taker itself. */
async function topMakers(
	client: VelocityClient,
	marketIndex: number,
	takerIsLong: boolean,
	taker: PublicKey
): Promise<MakerInfo[]> {
	const side = takerIsLong ? 'ask' : 'bid';
	const url = `${DLOB_URL}/topMakers?marketIndex=${marketIndex}&marketType=perp&side=${side}&limit=${MAKERS_PER_PLACEMENT}`;
	const makers = ((await (await fetch(url)).json()) as string[])
		.map((key) => new PublicKey(key))
		.filter((maker) => !maker.equals(taker));

	const accounts = await Promise.all(
		makers.map((maker) => client.program.account.user.fetch(maker))
	);

	return makers.map((maker, index) => {
		const makerUserAccount = accounts[index] as UserAccount;
		return {
			maker,
			makerStats: getUserStatsAccountPublicKey(
				client.program.programId,
				makerUserAccount.authority
			),
			makerUserAccount,
		};
	});
}

/**
 * The quoter section: the slab, then the union of the consulted slots' accounts. A slot is
 * consulted when it is the market's book or the taker's route names it. Mirrors velocity-rs
 * `quoter_cpi_section`.
 */
async function quoterSection(
	client: VelocityClient,
	marketIndex: number,
	route: PublicKey[]
): Promise<AccountMeta[]> {
	const slabKey = getQuoterSlabPublicKey(client.program.programId, marketIndex);
	const slabInfo = await client.connection.getAccountInfo(slabKey);
	if (!slabInfo) throw new Error(`no quoter slab for market ${marketIndex}`);

	const { clobMarket } = await client.getClobAccounts(marketIndex);
	const writableByKey = new Map<string, boolean>();
	const mark = (key: PublicKey, writable: boolean) =>
		writableByKey.set(
			key.toBase58(),
			(writableByKey.get(key.toBase58()) ?? false) || writable
		);

	for (const slot of decodeQuoterSlab(slabInfo.data).slots) {
		const quotes =
			!slot.entry.equals(PublicKey.default) &&
			!slot.suspended &&
			slot.config.isActive;
		const consulted =
			slot.config.responseAccount.equals(clobMarket) ||
			route.some((entry) => entry.equals(slot.entry));
		if (!quotes || !consulted) continue;

		slot.config.accounts
			.slice(0, slot.config.accountsCount)
			.forEach((meta) => mark(meta.pubkey, meta.isWritable));
		mark(slot.config.responseAccount, true);
		mark(slot.config.programId, false);
	}

	return [
		{ pubkey: slabKey, isSigner: false, isWritable: false },
		...[...writableByKey.entries()]
			.sort(([a], [b]) =>
				Buffer.compare(new PublicKey(a).toBuffer(), new PublicKey(b).toBuffer())
			)
			.map(([key, isWritable]) => ({
				pubkey: new PublicKey(key),
				isSigner: false,
				isWritable,
			})),
	];
}

/** Polls swift's `/attest` through its hold window. `undefined` when swift refuses. */
async function attest(
	orderSignature: string
): Promise<FlowAttestationV0 | undefined> {
	for (let attempt = 0; attempt < ATTEST_ATTEMPTS; attempt++) {
		const res = await fetch(`${SWIFT_URL}/attest`, {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ orderSignature }),
		});
		const body = await res.text();

		if (res.status === 425) {
			const { retryAfterMs } = JSON.parse(body) as { retryAfterMs?: number };
			await new Promise((resolve) => setTimeout(resolve, retryAfterMs ?? 250));
			continue;
		}

		if (!res.ok) {
			console.warn(`attest refused (${res.status}): ${body}`);
			return undefined;
		}

		const attested = JSON.parse(body) as {
			signature: string;
			expiryTs: number;
		};
		return {
			signature: Array.from(Buffer.from(attested.signature, 'base64')),
			expiryTs: new BN(attested.expiryTs),
		};
	}

	return undefined;
}

async function place(client: VelocityClient, order: SwiftOrder): Promise<void> {
	const takerAuthority = new PublicKey(order.taker_authority);
	const signingAuthority = new PublicKey(order.signing_authority);
	const isDelegateSigner = !signingAuthority.equals(takerAuthority);
	const message = client.decodeSignedMsgOrderParamsMessage(
		Buffer.from(order.order_message, 'hex'),
		isDelegateSigner
	);

	const params = message.signedMsgOrderParams;
	const taker = isDelegateSigner
		? (message as SignedMsgOrderParamsDelegateMessage).takerPubkey
		: getUserAccountPublicKeySync(
				client.program.programId,
				takerAuthority,
				(message as SignedMsgOrderParamsMessage).subAccountId
		  );
	const takerUserAccount = (await client.program.account.user.fetch(
		taker
	)) as UserAccount;

	const flowAttestation = await attest(order.order_signature);
	// A bumped book takes an unattested message only from the taker's own signer.
	if (!flowAttestation) {
		console.warn(`${order.uuid}: no attestation, skipping`);
		return;
	}

	const makers = await topMakers(
		client,
		params.marketIndex,
		'long' in params.direction,
		taker
	);
	const cuLimit = ComputeBudgetProgram.setComputeUnitLimit({
		units: 1_400_000,
	});
	const [placeIx] = await client.getPlaceSignedMsgTakerPerpOrderIxs(
		{
			orderParams: Buffer.from(order.order_message),
			signature: Buffer.from(order.order_signature, 'base64'),
		},
		params.marketIndex,
		{
			taker,
			takerStats: getUserStatsAccountPublicKey(
				client.program.programId,
				takerAuthority
			),
			takerUserAccount,
			signingAuthority,
		},
		[],
		undefined,
		undefined,
		undefined,
		flowAttestation,
		makers
	);
	placeIx.keys.push(
		...(await quoterSection(client, params.marketIndex, message.route ?? []))
	);

	const signature = await send(client, [cuLimit, placeIx]);
	if (signature) {
		console.log(
			`${order.uuid}: placed ${signature} with ${makers.length} makers`
		);
	}
}

const V1_VERSION_PREFIX = 0x81;
const V1_COMPUTE_UNIT_LIMIT_BIT = 0b100;
const V1_LOADED_ACCOUNTS_DATA_SIZE_BIT = 0b1000;
const V1_LOADED_ACCOUNTS_DATA_SIZE = 12 * 1024 * 1024;
const V1_DEFAULT_COMPUTE_UNITS = 1_400_000;

/** The compute unit limit a `SetComputeUnitLimit` instruction in `instructions` asks for. */
function computeUnitLimit(instructions: TransactionInstruction[]): number {
	const limit = instructions.find(
		(ix) =>
			ix.programId.equals(ComputeBudgetProgram.programId) && ix.data[0] === 2
	);
	return limit ? limit.data.readUInt32LE(1) : V1_DEFAULT_COMPUTE_UNITS;
}

/**
 * Serializes a signed v1 transaction, the layout `solana_message::v1` writes. A v1 message holds
 * 4096 bytes and 64 accounts with no lookup table, which a placement that routes to two PropAMMs
 * needs. Its compute budget lives in a config ahead of the instructions, so the compute budget
 * instructions move there. web3.js compiles the keys, by the same ordering rules.
 */
function signedV1Transaction(
	payer: Keypair,
	instructions: TransactionInstruction[],
	blockhash: string
): { wire: Buffer; signature: string } {
	const budget = instructions.filter((ix) =>
		ix.programId.equals(ComputeBudgetProgram.programId)
	);
	const compiled = new TransactionMessage({
		payerKey: payer.publicKey,
		recentBlockhash: blockhash,
		instructions: instructions.filter((ix) => !budget.includes(ix)),
	}).compileToV0Message();
	if (compiled.header.numRequiredSignatures !== 1) {
		throw new Error('a placement must have the placer as its only signer');
	}

	const u32 = (value: number) => {
		const out = Buffer.alloc(4);
		out.writeUInt32LE(value);
		return out;
	};
	const u16 = (value: number) => {
		const out = Buffer.alloc(2);
		out.writeUInt16LE(value);
		return out;
	};
	const ixs = compiled.compiledInstructions;
	const message = Buffer.concat([
		Buffer.from([
			V1_VERSION_PREFIX,
			compiled.header.numRequiredSignatures,
			compiled.header.numReadonlySignedAccounts,
			compiled.header.numReadonlyUnsignedAccounts,
		]),
		u32(V1_COMPUTE_UNIT_LIMIT_BIT | V1_LOADED_ACCOUNTS_DATA_SIZE_BIT),
		bs58.decode(blockhash),
		Buffer.from([ixs.length, compiled.staticAccountKeys.length]),
		...compiled.staticAccountKeys.map((key) => key.toBuffer()),
		u32(computeUnitLimit(budget)),
		u32(V1_LOADED_ACCOUNTS_DATA_SIZE),
		...ixs.map((ix) =>
			Buffer.concat([
				Buffer.from([ix.programIdIndex, ix.accountKeyIndexes.length]),
				u16(ix.data.length),
			])
		),
		...ixs.map((ix) =>
			Buffer.concat([Buffer.from(ix.accountKeyIndexes), Buffer.from(ix.data)])
		),
	]);

	const signature = nacl.sign.detached(message, payer.secretKey);
	return {
		wire: Buffer.concat([message, Buffer.from(signature)]),
		signature: bs58.encode(signature),
	};
}

/** The logs of a landed transaction. web3.js cannot parse a v1 transaction, so this asks the RPC directly. */
async function transactionLogs(
	client: VelocityClient,
	signature: string
): Promise<string[] | undefined> {
	const response = await fetch(client.connection.rpcEndpoint, {
		method: 'POST',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify({
			jsonrpc: '2.0',
			id: 1,
			method: 'getTransaction',
			params: [
				signature,
				{ commitment: 'confirmed', maxSupportedTransactionVersion: 1 },
			],
		}),
	});
	const { result } = (await response.json()) as {
		result?: { meta?: { logMessages?: string[] } };
	};
	return result?.meta?.logMessages;
}

/** Sends a v1 transaction and prints its logs when it fails. */
async function send(
	client: VelocityClient,
	instructions: TransactionInstruction[]
): Promise<string | undefined> {
	const { blockhash } = await client.connection.getLatestBlockhash();
	const payer = (client.wallet as unknown as { payer: Keypair }).payer;
	const { wire, signature } = signedV1Transaction(
		payer,
		instructions,
		blockhash
	);

	await client.connection.sendRawTransaction(wire);
	const result = await client.connection.confirmTransaction(
		signature,
		'confirmed'
	);
	if (!result.value.err) return signature;

	console.warn(
		`placement ${signature} failed`,
		await transactionLogs(client, signature)
	);
	return undefined;
}

/** Connects to swift's websocket, answers its nonce, and subscribes to every perp market. */
function subscribe(client: VelocityClient): void {
	const identity = Keypair.generate();
	const wsUrl = `${SWIFT_WS_URL}/ws?pubkey=${identity.publicKey.toBase58()}`;
	const ws = new WebSocket(wsUrl);

	ws.onmessage = (event) => {
		const message = JSON.parse(String(event.data));

		if (message.channel === 'auth' && message.nonce != null) {
			const signature = nacl.sign.detached(
				Buffer.from(message.nonce, 'utf-8'),
				identity.secretKey
			);
			ws.send(
				JSON.stringify({
					pubkey: identity.publicKey.toBase58(),
					signature: Buffer.from(signature).toString('base64'),
				})
			);
			return;
		}

		if (message.channel === 'auth' && message.message === 'Authenticated') {
			for (const market of DevnetPerpMarkets) {
				ws.send(
					JSON.stringify({
						action: 'subscribe',
						market_type: 'perp',
						market_name: market.symbol,
					})
				);
			}

			console.log('subscribed to swift orders');
			return;
		}

		if (message.order) {
			place(client, message.order as SwiftOrder).catch((err) =>
				console.warn(`${message.order.uuid}: ${err}`)
			);
		}
	};

	ws.onclose = () => {
		console.warn('swift websocket closed, reconnecting');
		setTimeout(() => subscribe(client), RECONNECT_MS);
	};
}

async function main(): Promise<void> {
	const client = await openPlacer(new Connection(RPC_URL, 'confirmed'));
	subscribe(client);
}

main().catch((err) => {
	console.error(err);
	process.exit(1);
});
