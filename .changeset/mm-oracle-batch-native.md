---
'@velocity-exchange/sdk': minor
---

Add `getUpdateMmOracleBatchNativeIx` / `updateMmOracleBatchNative`, builders for the program's new
batched MM-oracle native instruction (dispatch opcode 2), plus the `MmOracleBatchUpdate` entry type
and the `MM_ORACLE_BATCH_MAX_MARKETS` constant.

One instruction writes the MM oracle for many perp markets, so a caller pays one transaction
signature for the whole set instead of one per market, and the instruction's authentication prologue
(which is most of its compute cost) is paid once rather than per market.

Per-market rate-limit and sanity rejections skip only that market; the rest of the batch still
lands. Each entry carries its own market index, which the program re-checks against the account it
was paired with, so a misordered list fails loudly instead of writing one market's price onto
another. The builder additionally rejects an empty list, more than `MM_ORACLE_BATCH_MAX_MARKETS`
markets, duplicate market indexes, and non-positive prices (`BN` little-endian serialization drops
the sign, so a negative price would otherwise reach the program as its magnitude).
