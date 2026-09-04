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

**The slab signs everything.** The market's slab PDA is the one identity velocity signs every
external quoter CPI as: a book's `place_authority`, a midpoint instance's `execute_authority`,
and the signer slot in every registered CPI account list. `getClobAuthorityPublicKey`,
`getQuoterSignerPublicKey` and the `VelocityClient` accessors are removed, every CLOB
instruction drops its `clobAuthority` account, and fills stop carrying per-quoter signer
accounts — one more account lock back per quoter, and one per CLOB instruction.

**Client surface.** New permissionless `initializeQuoterSlab({ marketIndex })` creates a
one-slot slab; approval right-sizes the account from then on (growth paid by the admin,
trailing vacancy refunded on revocation), so there is no extend instruction. The header records
the slab's own bump (`QuoterSlabV0Account.bump`). New `getQuoterSlabPublicKey` (seeds `["quoter_slab", marketIndex]`),
`decodeQuoterSlab` (the slot region is raw bytes past the header, so the generated coder cannot
read it), and `VelocityClient.getQuoterSlabAccount`. Type mirrors: `QuoterConfigV0`,
`QuoterSlotV0`, `QuoterSlabV0Account`; `QuoterV0Account` becomes `{ config, padding }`.

**Breaking for transaction builders.** Every CLOB order instruction renames its `quoter` account
to `quoterSlab` (the slab PDA), and `clobProgram` is pinned to velocity's CLOB program id. Router
fills and `quoteRouter` carry the slab in the account tail instead of `QuoterV0` entries — a slot
is consulted when its response account rides the transaction — and `quoteRouter` drops its
`quoterCount` argument. `crankCrossMatch`'s leg indexes become slab slot indexes (the book is
slot 0), and `crankCrossMatch` names the perp market and the slab in its accounts struct.
Every endpoint this branch added takes a single args struct
(`PlaceAndTakePerpOrderV1Args`, `TriggerMarketOrderV1Args`, `UpdateQuoterApprovedArgs`, …);
`fillLegacyDlobOrder` moves `marketIndex` first and drops the dead `makerOrderId`. Errors
`QuoterSlabFull` (6404) and `QuoterNotOnSlab` (6405) are appended.
