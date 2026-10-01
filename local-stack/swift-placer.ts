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
	VersionedTransaction,
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

/** Sends a v0 transaction and prints its logs when it fails. */
async function send(
	client: VelocityClient,
	instructions: TransactionInstruction[]
): Promise<string | undefined> {
	const { blockhash } = await client.connection.getLatestBlockhash();
	const tx = new VersionedTransaction(
		new TransactionMessage({
			payerKey: client.wallet.publicKey,
			recentBlockhash: blockhash,
			instructions,
		}).compileToV0Message()
	);
	tx.sign([(client.wallet as unknown as { payer: Keypair }).payer]);

	const signature = await client.connection.sendRawTransaction(tx.serialize());
	const result = await client.connection.confirmTransaction(
		signature,
		'confirmed'
	);
	if (!result.value.err) return signature;

	const landed = await client.connection.getTransaction(signature, {
		commitment: 'confirmed',
		maxSupportedTransactionVersion: 0,
	});
	console.warn(`placement ${signature} failed`, landed?.meta?.logMessages);
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
