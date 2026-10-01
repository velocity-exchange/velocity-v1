// bun test deploy-scripts/upgrade-guards.test.ts
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'crypto';
import { PublicKey, TransactionInstruction } from '@solana/web3.js';
import {
	asProposed,
	executableHash,
	instructionKey,
	liftedStatus,
	proposalDoes,
	proposalWithin,
	UPGRADE_PAUSE_BITS,
} from './upgrade-guards';
import {
	exchangeStatusGuardIx,
	exchangeStatusWriteProblem,
} from '../packages/cli-admin/src/lib/exchangeStatusGuard';

const velocity = PublicKey.unique();
const state = PublicKey.unique();
const vault = PublicKey.unique();

function ix(data: number[], accounts: PublicKey[]): TransactionInstruction {
	return new TransactionInstruction({
		programId: velocity,
		keys: accounts.map((pubkey) => ({
			pubkey,
			isSigner: false,
			isWritable: true,
		})),
		data: Buffer.from(data),
	});
}

function updateExchangeStatus(status: number) {
	const discriminator = createHash('sha256')
		.update('global:update_exchange_status')
		.digest()
		.subarray(0, 8);
	return {
		program: velocity,
		data: Buffer.concat([discriminator, Buffer.from([status])]),
	};
}

test('the upgrade pause covers settle, withdraw, liquidation and funding', () => {
	assert.equal(UPGRADE_PAUSE_BITS, 16 | 2 | 64 | 32);
});

test('the lift clears the bracket bits and keeps a fill pause added later', () => {
	assert.equal(liftedStatus(UPGRADE_PAUSE_BITS), 0);
	assert.equal(liftedStatus(UPGRADE_PAUSE_BITS | 8), 8);
	assert.equal(liftedStatus(UPGRADE_PAUSE_BITS | 1 | 128), 1 | 128);
});

test('a proposal does a step only with the same data and accounts in order', () => {
	const step = [ix([1, 2], [state, vault])];
	assert.ok(proposalDoes(step.map(asProposed), step));
	assert.ok(!proposalDoes([ix([1, 3], [state, vault])].map(asProposed), step));
	assert.ok(!proposalDoes([ix([1, 2], [vault, state])].map(asProposed), step));
	assert.ok(
		!proposalDoes([...step, ix([9], [vault])].map(asProposed), step),
		'an extra instruction hidden behind a matching one does not count'
	);
});

test('a sync proposal counts only when every instruction is an expected sync', () => {
	const syncs = [ix([5, 1], [state]), ix([5, 2], [state])];
	const keys = new Set(syncs.map((sync) => instructionKey(asProposed(sync))));
	assert.ok(proposalWithin([syncs[1]].map(asProposed), keys));
	assert.ok(
		!proposalWithin([syncs[0], ix([5, 9], [state])].map(asProposed), keys)
	);
	assert.ok(!proposalWithin([], keys));
});

test('the executable hash ignores trailing zero padding', () => {
	const elf = Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0, 7]);
	const padded = Buffer.concat([elf, Buffer.alloc(64)]);
	assert.equal(executableHash(padded), executableHash(elf));
	assert.equal(
		executableHash(elf),
		createHash('sha256').update(elf).digest('hex')
	);
});

test('a status write built from another status is refused at execution', () => {
	const lift = [
		asProposed(exchangeStatusGuardIx(UPGRADE_PAUSE_BITS)),
		updateExchangeStatus(0),
	];
	assert.equal(
		exchangeStatusWriteProblem(lift, velocity, UPGRADE_PAUSE_BITS),
		undefined
	);
	assert.match(
		exchangeStatusWriteProblem(lift, velocity, UPGRADE_PAUSE_BITS | 8) ?? '',
		/Propose it again/
	);
});

test('a status write without a guard is refused when it clears a live bit', () => {
	assert.equal(
		exchangeStatusWriteProblem([updateExchangeStatus(9)], velocity, 8),
		undefined
	);
	assert.match(
		exchangeStatusWriteProblem([updateExchangeStatus(0)], velocity, 8) ?? '',
		/clear bits 8/
	);
	assert.equal(exchangeStatusWriteProblem([], velocity, 8), undefined);
});
