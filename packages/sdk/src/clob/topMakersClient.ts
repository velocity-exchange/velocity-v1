import { PublicKey } from '@solana/web3.js';
import { MakerInfo, PositionDirection, isVariant } from '../types';
import { getUserStatsAccountPublicKey } from '../addresses/pda';

/** The `User` accounts a fill needs. `UserMap` satisfies this. */
export type MakerAccountGetter = {
	mustGet(key: string): Promise<{ getUserAccountOrThrow(): any }>;
};

/** How long to wait on the dlob-server before giving up on the book's makers. */
const REQUEST_TIMEOUT_MS = 2_000;

/**
 * The book's best resting owners, read from the dlob-server's `/topMakers`.
 *
 * A routed placement fills against the book in the same instruction, so every
 * maker it can reach must ride the transaction. A placement that leaves one out
 * is refused on chain.
 */
export class TopMakersClient {
	private readonly url: string;

	/** @param url - dlob-server base URL, for example `https://dlob.velocity.trade`. */
	constructor(url: string) {
		this.url = url.replace(/\/$/, '');
	}

	/**
	 * The `MakerInfo` list for the side `direction` takes. A long takes the asks.
	 *
	 * Returns an empty list when the request fails, and skips a maker whose
	 * account does not load. A caller that reaches fewer makers loses their
	 * depth. It does not lose the fill, because the remainder rests on the book.
	 *
	 * @param programId - The velocity program id, which derives each maker's `UserStats`.
	 * @param limit - How many makers to ask for.
	 */
	public async fetchMakerInfos(
		programId: PublicKey,
		userMap: MakerAccountGetter,
		marketIndex: number,
		direction: PositionDirection,
		limit: number
	): Promise<MakerInfo[]> {
		const side = isVariant(direction, 'long') ? 'ask' : 'bid';
		const query = new URLSearchParams({
			marketType: 'perp',
			marketIndex: String(marketIndex),
			side,
			limit: String(limit),
		});

		const keys = await this.fetchMakerKeys(query);
		const makerInfos: MakerInfo[] = [];

		for (const key of keys) {
			try {
				const makerUserAccount = (
					await userMap.mustGet(key)
				).getUserAccountOrThrow();

				makerInfos.push({
					maker: new PublicKey(key),
					makerStats: getUserStatsAccountPublicKey(
						programId,
						makerUserAccount.authority
					),

					makerUserAccount,
				});
			} catch (e) {
				console.warn(`TopMakersClient: skipping maker ${key}: ${e}`);
			}
		}

		return makerInfos;
	}

	private async fetchMakerKeys(query: URLSearchParams): Promise<string[]> {
		const controller = new AbortController();
		const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);

		try {
			const response = await fetch(`${this.url}/topMakers?${query}`, {
				signal: controller.signal,
			});

			if (!response.ok) {
				console.warn(`TopMakersClient: status ${response.status}`);

				return [];
			}

			const body = await response.json();

			return Array.isArray(body) ? (body as string[]) : [];
		} catch (e) {
			console.warn(`TopMakersClient: request failed: ${e}`);

			return [];
		} finally {
			clearTimeout(timer);
		}
	}
}
