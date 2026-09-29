// bun test deploy-scripts/relay-watch.test.ts
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PublicKey, SystemProgram } from '@solana/web3.js';
import {
	classifyWatches,
	decodeWatch,
	ExpectedWatch,
	parseWatchCreators,
	WATCH_V0_LEN,
} from './relay-watch';

const velocity = PublicKey.unique();
const payer = PublicKey.unique();
const target = PublicKey.unique();

function watchData(
	targetProgram: PublicKey,
	creator: PublicKey,
	blockOffset: number
): Buffer {
	const data = Buffer.alloc(WATCH_V0_LEN);
	targetProgram.toBuffer().copy(data, 8);
	target.toBuffer().copy(data, 40);
	creator.toBuffer().copy(data, 72);
	data.writeUInt32LE(blockOffset, 104);
	return data;
}

const expected: ExpectedWatch = {
	target,
	targetOwner: velocity,
	blockOffset: 8,
	creators: [payer],
};

function classify(...watches: Buffer[]) {
	return classifyWatches(
		watches.map((data) => decodeWatch(PublicKey.unique(), data)!),
		expected
	);
}

test('the payer watch at the block offset serves the target', () => {
	const { serving, impostors } = classify(watchData(velocity, payer, 8));
	assert.equal(serving.length, 1);
	assert.equal(impostors.length, 0);
});

test('a watch by another creator does not serve the target', () => {
	const { serving, impostors } = classify(watchData(velocity, PublicKey.unique(), 8));
	assert.equal(serving.length, 0);
	assert.equal(impostors.length, 1);
});

test('a watch at another offset does not serve the target', () => {
	const { serving } = classify(watchData(velocity, payer, 9));
	assert.equal(serving.length, 0);
});

test('a watch registered before the target existed does not serve it', () => {
	const { serving } = classify(watchData(SystemProgram.programId, payer, 8));
	assert.equal(serving.length, 0);
});

test('an impostor beside the real watch leaves the target served', () => {
	const { serving, impostors } = classify(
		watchData(velocity, PublicKey.unique(), 0),
		watchData(velocity, payer, 8)
	);
	assert.equal(serving.length, 1);
	assert.equal(impostors.length, 1);
});

test('the creator list always holds the fallback key once', () => {
	const other = PublicKey.unique();
	const creators = parseWatchCreators(
		`${payer.toBase58()}, ${other.toBase58()}`,
		payer
	);
	assert.deepEqual(
		creators.map((key) => key.toBase58()),
		[payer.toBase58(), other.toBase58()]
	);
	assert.equal(parseWatchCreators(undefined, payer).length, 1);
});
