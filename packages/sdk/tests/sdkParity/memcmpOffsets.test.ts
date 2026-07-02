import {
	getUserStatsIsReferredFilter,
	getUserStatsIsReferredOrReferrerFilter,
} from '../../src/memcmp';
import { assert } from 'chai';
import bs58 from 'bs58';

// Offset of `referrer_status` within the `UserStats` account, including the
// 8-byte Anchor discriminator: authority(32) + referrer(32) + fees(32) +
// maker/taker/filler volume 30d(24) + last maker/taker/filler volume 30d
// ts(24) + if_staked_quote_asset_amount(8) + number_of_sub_accounts(2) +
// number_of_sub_accounts_created(2) = 156, + 8 discriminator = 164.
const REFERRER_STATUS_OFFSET = 164;
const USER_STATS_SIZE = 240; // includes the 8-byte discriminator

function buildSyntheticUserStatsBuffer(referrerStatus: number): Buffer {
	const buffer = Buffer.alloc(USER_STATS_SIZE);
	buffer.writeUInt8(referrerStatus, REFERRER_STATUS_OFFSET);
	return buffer;
}

describe('UserStats memcmp offsets', () => {
	it('getUserStatsIsReferredFilter targets byte 164 (referrer_status), not the stale 188', () => {
		const filter = getUserStatsIsReferredFilter();

		assert.equal(filter.memcmp.offset, REFERRER_STATUS_OFFSET);
		assert.notEqual(filter.memcmp.offset, 188);
		assert.equal(bs58.decode(filter.memcmp.bytes as string)[0], 2);
	});

	it('getUserStatsIsReferredOrReferrerFilter targets byte 164, not the stale 188', () => {
		const filter = getUserStatsIsReferredOrReferrerFilter();

		assert.equal(filter.memcmp.offset, REFERRER_STATUS_OFFSET);
		assert.notEqual(filter.memcmp.offset, 188);
		assert.equal(bs58.decode(filter.memcmp.bytes as string)[0], 3);
	});

	it('a synthetic UserStats buffer with referrer_status=2 (IsReferred) matches only at the corrected offset', () => {
		const buffer = buildSyntheticUserStatsBuffer(2);
		const filter = getUserStatsIsReferredFilter();
		const expectedByte = bs58.decode(filter.memcmp.bytes as string)[0];

		assert.equal(buffer[filter.memcmp.offset as number], expectedByte);
		// The old hardcoded offset (188) lands on a zeroed padding byte, which is
		// exactly why the filter previously matched zero accounts.
		assert.equal(buffer[188], 0);
	});
});
