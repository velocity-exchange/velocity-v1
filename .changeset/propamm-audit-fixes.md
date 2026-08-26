---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

PropAMM audit fixes. Two are breaking for anything that builds these instructions by hand.

**The book authority is its own key.** Every CLOB instruction's `quoter_signer` account is now
`clob_authority`, derived from seeds `["clob_authority"]` — `getClobAuthorityPublicKey`, or
`VelocityClient.getClobAuthorityPublicKey()`. It is what a market's book is initialized with.

`getQuoterSignerPublicKey` still exists but now takes the registry entry and derives
`["quoter_signer", entry]`: the key velocity signs one entry's `quote_v0`/`execute_v0` CPI as. A
quoter registering its own account list uses its own entry's key, and a maker configuring a quoter
instance (the midpoint's `execute_authority`) points it there.

The split is the fix, not a rename. One global key served both roles, and a quoter's account list
has to carry the key its `execute_v0` authenticates against — so an approved quoter whose list also
named a book held the book's `place_authority` as a live signature inside its own CPI, and
`place_order_v0` takes the user it places for as an argument. Keying the quoter side per entry also
means the signature a quoter receives proves velocity called *that* quoter and nothing else.

**Approval requires a frozen program.** `update_quoter_approved` takes two more accounts,
`quoterProgram` and an optional `quoterProgramData`, and refuses to approve an entry whose program
still has an upgrade authority. Approving an upgradeable program approves its author, not its code.

**`RouterAllocation.scaledQuote`** is new: `Σ price · base` before the division into quote units,
which is the scalar the program holds a fill to. A client predicting whether a fill will be accepted
needs it.

Also: `min_cross_surplus` must be above zero when attaching a CLOB to a market (the admin CLI's
`--min-cross-surplus` no longer defaults to `0`), and `splitAcrossQuoters` now reports `scaledQuote`
per allocation.

**Withheld depth no longer holds back taker size.** A book that stops its walk at
an order whose owner the transaction omits used to reserve taker size equal to
that depth, and then discard it, so the taker underfilled. The size now goes to
the sources that can fill it. `splitAcrossQuoters` loses its `reserve`
parameter, and `RouterReserve` is removed.

What replaces it is an obligation on whoever built the transaction. When a book
withholds depth and the taker did not sign, `fill_perp_order` and
`fill_perp_order_v1` require that the transaction was full and that every loaded
user did something. Three new errors say which rule failed:
`FillerOmittedReachableMaker` (6395), `FillerPaddedTheUserSet` (6396), and
`FillerObligationUncountable` (6397).

Both fill instructions gain an optional `instructions_sysvar` account. A fill
needs it only to be counted, so a taker filling its own order can omit it — but
a filler that omits it is refused whenever a book withholds. The SDK and
`velocity-rs` builders always pass it.
