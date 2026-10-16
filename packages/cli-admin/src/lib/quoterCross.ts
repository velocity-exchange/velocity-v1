/** Relay cranks no cross against a Custom quoter until its cross conditions
 * exist. Activation arms them in the program. Approval and a book attach add
 * the attach here, because either one can be the moment the entry qualifies. */
import { AnchorProvider } from '@coral-xyz/anchor';
import {
	PublicKey,
	SystemProgram,
	TransactionInstruction,
} from '@solana/web3.js';
import {
	decodeQuoterSlab,
	getClobCrankConditionsPublicKey,
	getInitializeQuoterCrossConditionsIx,
	getPerpMarketPublicKeySync,
	getQuoterCrossConditionsPublicKey,
	getQuoterSlabPublicKey,
	getRegisterWatchIxs,
	QUOTER_CROSS_BLOCK_OFFSET,
	QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS,
	quotingCustomEntries,
	RELAY_WATCH_V0_LEN,
	VelocityProgram,
} from '@velocity-exchange/sdk';
import {
	DispatchResult,
	isDryRun,
	reportDispatch,
	sendOrPropose,
} from './squads';

export function isCustomEntry(quoterType: unknown): boolean {
	return Boolean(
		quoterType && 'custom' in (quoterType as Record<string, unknown>)
	);
}

type CrossAttach = {
	crossConditions: PublicKey;
	watch: PublicKey;
	attach: TransactionInstruction;
	/** Empty when the payer already registered the watch. */
	registerWatch: TransactionInstruction[];
};

export async function crossAttachIxs(
	provider: AnchorProvider,
	program: VelocityProgram,
	quoter: PublicKey,
	marketIndex: number,
	payer: PublicKey,
	expireFallbackSlots: number
): Promise<CrossAttach> {
	const crossConditions = getQuoterCrossConditionsPublicKey(
		program.programId,
		quoter
	);
	const attach = await getInitializeQuoterCrossConditionsIx(program, {
		payer,
		quoter,
		marketIndex,
		expireFallbackSlots,
	});
	const { watch, ixs } = await getRegisterWatchIxs({
		payer,
		target: crossConditions,
		blockOffset: QUOTER_CROSS_BLOCK_OFFSET,
		seed: `cross-${quoter.toBase58().slice(0, 26)}`,
		rentLamports: await provider.connection.getMinimumBalanceForRentExemption(
			RELAY_WATCH_V0_LEN
		),
	});
	const watched = !!(await provider.connection.getAccountInfo(watch));
	return { crossConditions, watch, attach, registerWatch: watched ? [] : ixs };
}

async function bookAttached(
	program: VelocityProgram,
	marketIndex: number
): Promise<boolean> {
	const marketConditions = await (
		program.account as any
	).clobCrankConditionsV0.fetchNullable(
		getClobCrankConditionsPublicKey(program.programId, marketIndex)
	);
	return !!marketConditions && marketConditions.topOfBookLen > 0;
}

export type QuotingEntry = {
	quoter: PublicKey;
	market: number;
	isActive: boolean;
};

/**
 * The cross attach for an entry that a transaction approves, or none when the
 * program would refuse it. A proposal leaves out the watch,
 * because `migrate.ts` counts only a watch that its own payer made.
 */
export async function entryCrossIxs(
	provider: AnchorProvider,
	program: VelocityProgram,
	entry: QuotingEntry,
	payer: PublicKey,
	registerWatch: boolean
): Promise<TransactionInstruction[]> {
	const blocker = !entry.isActive
		? 'the entry is inactive'
		: !(await bookAttached(program, entry.market))
		? "the market's CLOB is not attached"
		: null;
	if (blocker) {
		console.log(
			`cross conditions not attached: ${blocker}. Run \`quoter attach-cross\` or migrate.ts once it can attach.`
		);
		return [];
	}

	const cross = await crossAttachIxs(
		provider,
		program,
		entry.quoter,
		entry.market,
		payer,
		QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS
	);
	console.log(
		`cross conditions ${cross.crossConditions.toBase58()}: attaching`
	);
	return registerWatch
		? [cross.attach, ...cross.registerWatch]
		: [cross.attach];
}

type CrossArmingAccounts = {
	perpMarket: PublicKey | null;
	marketConditions: PublicKey | null;
	crossConditions: PublicKey | null;
	systemProgram: PublicKey | null;
};

export type CrossArming = {
	accounts: CrossArmingAccounts;
	registerWatch: TransactionInstruction[];
};

export const NO_CROSS_ARMING: CrossArming = {
	accounts: {
		perpMarket: null,
		marketConditions: null,
		crossConditions: null,
		systemProgram: null,
	},
	registerWatch: [],
};

/**
 * The accounts `update_quoter_active` arms an approved Custom entry's cross
 * conditions with. An entry without a slab slot arms nothing and passes none.
 * The program reads `marketConditions` to learn whether the book is attached,
 * and it refuses the rest while the book is not.
 */
export async function activationCrossArming(
	provider: AnchorProvider,
	program: VelocityProgram,
	quoter: PublicKey,
	marketIndex: number,
	authority: PublicKey
): Promise<CrossArming> {
	const slab = await provider.connection.getAccountInfo(
		getQuoterSlabPublicKey(program.programId, marketIndex)
	);
	const holdsSlot =
		!!slab &&
		decodeQuoterSlab(slab.data).slots.some((slot) => slot.entry.equals(quoter));
	if (!holdsSlot) return NO_CROSS_ARMING;

	const marketConditions = getClobCrankConditionsPublicKey(
		program.programId,
		marketIndex
	);
	if (!(await bookAttached(program, marketIndex))) {
		return {
			accounts: { ...NO_CROSS_ARMING.accounts, marketConditions },
			registerWatch: [],
		};
	}

	const cross = await crossAttachIxs(
		provider,
		program,
		quoter,
		marketIndex,
		authority,
		QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS
	);
	return {
		accounts: {
			perpMarket: getPerpMarketPublicKeySync(program.programId, marketIndex),
			marketConditions,
			crossConditions: cross.crossConditions,
			systemProgram: SystemProgram.programId,
		},
		registerWatch: cross.registerWatch,
	};
}

export type MarketCrossGroup = {
	quoter: PublicKey;
	ixs: TransactionInstruction[];
};

/**
 * One cross attach group for every active, approved Custom entry on the
 * market. A book attach changes the payment and the region these copy, so a
 * group also re-prices an entry that already has its conditions.
 */
export async function marketCrossGroups(
	provider: AnchorProvider,
	program: VelocityProgram,
	marketIndex: number,
	payer: PublicKey,
	registerWatch: boolean
): Promise<MarketCrossGroup[]> {
	const slab = await provider.connection.getAccountInfo(
		getQuoterSlabPublicKey(program.programId, marketIndex)
	);
	if (!slab) return [];

	const groups: MarketCrossGroup[] = [];
	for (const quoter of quotingCustomEntries(slab.data)) {
		const cross = await crossAttachIxs(
			provider,
			program,
			quoter,
			marketIndex,
			payer,
			QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS
		);
		groups.push({
			quoter,
			ixs: registerWatch
				? [cross.attach, ...cross.registerWatch]
				: [cross.attach],
		});
	}

	return groups;
}

/** Send each group of `marketCrossGroups` in its own transaction, after the
 * book attach has landed. */
export async function sendMarketCrossGroups(
	provider: AnchorProvider,
	groups: MarketCrossGroup[],
	marketIndex: number
): Promise<void> {
	// A dry run never lands the attach, so a simulation of these would fail.
	if (isDryRun()) {
		for (const { quoter } of groups) {
			console.log(
				`market ${marketIndex}: cross conditions for quoter ${quoter.toBase58()} sent after the attach`
			);
		}

		return;
	}

	for (const { quoter, ixs } of groups) {
		const result: DispatchResult = await sendOrPropose(
			provider,
			ixs,
			undefined,
			`velocity-admin cross conditions market ${marketIndex}`
		);
		reportDispatch(
			`market ${marketIndex}: cross conditions for quoter ${quoter.toBase58()}`,
			result
		);
	}
}
