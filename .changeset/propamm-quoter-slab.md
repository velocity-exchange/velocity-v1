---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

The quoter registry's approved set moves into one `QuoterSlabV0` account per market.

**Why.** A router fill spent one account lock per quoter on its `QuoterV0` entry alone, against
the 64 a transaction can name. The slab holds every approved config in one account, so a quoter
costs two unshared locks (its program and its response account) plus the one slab every quoter
shares.

**The registry is now two halves.** `QuoterV0` is the *staging* entry: the maker's proposal, and
the quoter's identity (signed routes, `PerpMarket.clobQuoter` and relay conditions still name the
entry pubkey). Nothing fills from it. `updateQuoterApproved` copies the staged config into a slab
slot, and fills read only that copy — so a staging edit stays inert until the admin copies again,
and the previously vetted copy keeps serving meanwhile. There is no `isApproved` flag any more;
approval is presence in the slab. Slot 0 is reserved for the market's book; `Custom` quoters
occupy slots 1+. Revoking a `Custom` slot clears it; revoking the book suspends it, so cancel and
removal paths keep working on a dead book. `updateQuoterActive`, `updateQuoterPriority` and
`updateQuoterMaxOracleDeviation` write through to the live slot when the slab is passed.

**One account list per quoter.** The two per-leg 32-slot lists collapse into one 12-slot list
plus per-leg index lists. `updateQuoterAccounts` takes `{ metas, quoteIndexes, executeIndexes }`
in one call; the chunked `leg`/`index` form and `QuoterCpiLeg` are removed.

**Client surface.** New permissionless `initializeQuoterSlab(marketIndex, capacity)` and
`extendQuoterSlab(marketIndex, capacity)` — capacity is the account's size (recorded in the
header), not a layout constant. One call allocates or adds at most 13 slots (the runtime's
10,240-byte ceiling); extension appends vacant slots and never moves an occupied one. New `getQuoterSlabPublicKey` (seeds `["quoter_slab", marketIndex]`),
`decodeQuoterSlab` (the slot region is raw bytes past the header, so the generated coder cannot
read it), and `VelocityClient.getQuoterSlabAccount`. Type mirrors: `QuoterConfigV0`,
`QuoterSlotV0`, `QuoterSlabV0Account`; `QuoterV0Account` becomes `{ config, padding }`.

**Breaking for transaction builders.** Every CLOB order instruction renames its `quoter` account
to `quoterSlab` (the slab PDA), and `clobProgram` is pinned to velocity's CLOB program id. Router
fills and `quoteRouter` carry the slab in the account tail instead of `QuoterV0` entries — a slot
is consulted when its response account rides the transaction — and `quoteRouter` drops its
`quoterCount` argument. `crankCrossMatch`'s leg indexes become slab slot indexes (the book is
slot 0). Errors `QuoterSlabFull` (6404) and `QuoterNotOnSlab` (6405) are appended.
