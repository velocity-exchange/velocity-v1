/**
 * Decodes a taker's fills into legs and names the source of each: the vAMM, the book, or a
 * midpoint instance. A leg is one `OrderActionRecord` fill.
 */
import * as fs from 'fs';
import { BorshEventCoder, Idl } from '@coral-xyz/anchor';
import {
	Connection,
	ParsedTransactionWithMeta,
	PublicKey,
} from '@solana/web3.js';
import {
	decodeQuoterSlab,
	getQuoterSlabPublicKey,
	getUserAccountPublicKeySync,
	isVariant,
	VELOCITY_PROGRAM_ID,
} from '@velocity-exchange/sdk';

const coder = new BorshEventCoder(
	JSON.parse(
		fs.readFileSync('packages/sdk/src/idl/velocity.json', 'utf-8')
	) as Idl
);

export type FillLeg = {
	/** `vamm`, `clob`, or `propamm:<instance>`. */
	source: string;
	maker: string | null;
	base: number;
	price: number;
	signature: string;
};

function instanceMaker(instance: string): string | undefined {
	const file =
		instance === 'a'
			? '/state/keys/midpoint-maker.json'
			: `/state/keys/midpoint-${instance}-maker.json`;
	if (!fs.existsSync(file)) return undefined;

	const secret = Uint8Array.from(JSON.parse(fs.readFileSync(file, 'utf-8')));
	return getUserAccountPublicKeySync(
		new PublicKey(VELOCITY_PROGRAM_ID),
		new PublicKey(secret.slice(32)),
		0
	).toBase58();
}

/**
 * The source label of each maker the market's quoter slab names: `vamm` for the vAMM's user, and
 * `propamm:<instance>` for a midpoint instance's maker. A maker the slab does not name is a book
 * maker.
 */
export async function makerLabels(
	connection: Connection,
	marketIndex: number,
	instances: string[]
): Promise<Map<string, string>> {
	const byUser = new Map(
		instances.flatMap((instance) => {
			const user = instanceMaker(instance);
			return user ? [[user, `propamm:${instance}`] as const] : [];
		})
	);

	const slab = await connection.getAccountInfo(
		getQuoterSlabPublicKey(new PublicKey(VELOCITY_PROGRAM_ID), marketIndex)
	);
	if (!slab) throw new Error(`no quoter slab for market ${marketIndex}`);

	const labels = new Map<string, string>();
	for (const slot of decodeQuoterSlab(slab.data).slots) {
		const user = slot.config.user.toBase58();
		if (isVariant(slot.config.quoterType, 'vamm')) labels.set(user, 'vamm');
		if (isVariant(slot.config.quoterType, 'custom')) {
			labels.set(user, byUser.get(user) ?? 'propamm:?');
		}
	}

	return labels;
}

/** A vAMM fill names no maker, so its explanation marks it. */
function sourceOf(
	explanation: object,
	maker: string | null,
	labels: Map<string, string>
): string {
	if ('OrderFilledWithAMM' in explanation) return 'vamm';

	return (maker && labels.get(maker)) ?? 'clob';
}

/**
 * The base64 event in a log line. `emit!` writes `Program data:`, and `emit_stack` writes the
 * same bytes as `Program log:`.
 */
function eventPayload(log: string): string | undefined {
	const payload = log.replace(/^Program (data|log): /, '');
	return payload !== log && /^[A-Za-z0-9+/]{16,}={0,2}$/.test(payload)
		? payload
		: undefined;
}

/** Every fill in `transactions` that names `taker` as the taker, oldest transaction first. */
export function decodeFillLegs(
	transactions: (ParsedTransactionWithMeta | null)[],
	taker: PublicKey,
	labels: Map<string, string>
): FillLeg[] {
	return transactions
		.filter((tx): tx is ParsedTransactionWithMeta => !!tx)
		.sort((a, b) => a.slot - b.slot)
		.flatMap((tx) =>
			(tx.meta?.logMessages ?? [])
				.map(eventPayload)
				.filter((payload): payload is string => !!payload)
				.map((payload) => coder.decode(payload))
				.filter((event) => event?.name === 'OrderActionRecord')
				.map((event) => event!.data as Record<string, any>)
				.filter(
					(record) =>
						record.base_asset_amount_filled && record.taker?.equals(taker)
				)
				.map((record) => {
					const base = Number(record.base_asset_amount_filled) / 1e9;
					const quote = Number(record.quote_asset_amount_filled) / 1e6;
					const maker = record.maker?.toBase58() ?? null;
					return {
						source: sourceOf(record.action_explanation, maker, labels),
						maker,
						base,
						price: quote / base,
						signature: tx.transaction.signatures[0],
					};
				})
		);
}

export function formatLegs(legs: FillLeg[]): string {
	if (legs.length === 0) return 'no fills';

	const rows = legs.map(
		(leg) =>
			`  ${leg.source.padEnd(11)} ${leg.base
				.toFixed(4)
				.padStart(8)} @ ${leg.price
				.toFixed(4)
				.padStart(10)}  ${leg.signature.slice(0, 16)}`
	);
	const total = legs.reduce((sum, leg) => sum + leg.base, 0);
	return [
		'fills (source, base, price, transaction):',
		...rows,
		`  total ${total.toFixed(4)}`,
	].join('\n');
}
