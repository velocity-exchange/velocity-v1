/**
 * allow-verbose: the usage header of an operator script.
 *
 * Rewrite the dumped `User` accounts in the checkout's layout, before the validator loads them.
 *
 *   bun run local-stack/relayout-users.ts <account dir>
 *
 * Devnet still runs the layout with a `u8` `open_orders`. In the checkout's layout
 * `PerpPosition` is 88 bytes and `User` is 4560, and `extend_account` only appends zeros, so a
 * dumped account cannot grow in place. A 4376-byte account is first zero-padded to 4496, as
 * `migrate.ts` pads it on chain. An account already at 4560 bytes is left as it is.
 */
import { createHash } from 'crypto';
import { readdirSync, readFileSync, writeFileSync } from 'fs';
import path from 'path';

const USER_DISCRIMINATOR = createHash('sha256')
	.update('account:User')
	.digest()
	.subarray(0, 8);

const LEGACY_USER_LEN = 4376;
const DEVNET_USER_LEN = 4496;
const USER_LEN = 4560;

const PERP_POSITIONS_OFFSET = 8 + 32 + 32 + 32 + 8 * 40;
const DEVNET_PERP_POSITION_LEN = 80;
const PERP_POSITION_COUNT = 8;
const DEVNET_STATUS_OFFSET = 4468;

/** `(128 + data length) * 3480 * 2`, the test validator's rent-exempt minimum. */
const rentExempt = (len: number) => (128 + len) * 3480 * 2;

function perpPositionInNewLayout(position: Buffer): Buffer {
	const openOrders = position[78];
	const positionFlag = position[79];
	return Buffer.concat([
		position.subarray(0, 78),
		Buffer.from([openOrders, 0, positionFlag]),
		Buffer.alloc(7),
	]);
}

/** `has_open_order` moves ahead of `open_orders`, which becomes a `u16`. */
function tailFlagsInNewLayout(flags: Buffer): Buffer {
	const [status, margin, idle, openOrders, hasOpenOrder, ...rest] = flags;
	const [openAuctions, hasOpenAuction, poolId, specialUserStatus] = rest;
	return Buffer.from([
		status,
		margin,
		idle,
		hasOpenOrder,
		openOrders,
		0,
		openAuctions,
		hasOpenAuction,
		poolId,
		specialUserStatus,
		0,
		0,
	]);
}

function relayout(devnet: Buffer): Buffer {
	const positions = Array.from({ length: PERP_POSITION_COUNT }, (_, i) => {
		const start = PERP_POSITIONS_OFFSET + i * DEVNET_PERP_POSITION_LEN;
		return perpPositionInNewLayout(
			devnet.subarray(start, start + DEVNET_PERP_POSITION_LEN)
		);
	});

	const afterPositions =
		PERP_POSITIONS_OFFSET + PERP_POSITION_COUNT * DEVNET_PERP_POSITION_LEN;
	return Buffer.concat([
		devnet.subarray(0, PERP_POSITIONS_OFFSET),
		...positions,
		devnet.subarray(afterPositions, DEVNET_STATUS_OFFSET),
		tailFlagsInNewLayout(
			devnet.subarray(DEVNET_STATUS_OFFSET, DEVNET_STATUS_OFFSET + 12)
		),
		devnet.subarray(DEVNET_STATUS_OFFSET + 12),
	]);
}

function main(): void {
	const dir = process.argv[2];
	if (!dir) throw new Error('usage: relayout-users.ts <account dir>');

	let rewritten = 0;
	for (const name of readdirSync(dir)) {
		const file = path.join(dir, name);
		const dumped = JSON.parse(readFileSync(file, 'utf8'));
		const data = Buffer.from(dumped.account.data[0], 'base64');
		if (!data.subarray(0, 8).equals(USER_DISCRIMINATOR)) continue;

		if (data.length !== LEGACY_USER_LEN && data.length !== DEVNET_USER_LEN) {
			continue;
		}

		const devnet = Buffer.concat([
			data,
			Buffer.alloc(DEVNET_USER_LEN - data.length),
		]);
		const relaidOut = relayout(devnet);
		if (relaidOut.length !== USER_LEN) {
			throw new Error(`${name}: re-laid out to ${relaidOut.length} bytes`);
		}

		dumped.account.data[0] = relaidOut.toString('base64');
		dumped.account.space = USER_LEN;
		dumped.account.lamports = Math.max(
			dumped.account.lamports,
			rentExempt(USER_LEN)
		);
		writeFileSync(file, JSON.stringify(dumped));
		rewritten++;
	}

	console.log(`relayout-users: ${rewritten} User accounts rewritten`);
}

main();
