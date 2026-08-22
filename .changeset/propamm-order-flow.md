---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

PropAMM order flow: perps fill through a router across the vAMM, resting DLOB orders and external
quoter programs. `docs/DRIFT-TO-VELOCITY.md` is the migration reference; this is the surface.

**Fills.** Every perp fill goes through one router pass. Each source publishes discrete price
levels and the split walks priority tiers ascending — vAMM, then a book, then customs — pro rata
within a tier. AMM JIT is gone with `PerpFulfillmentMethod`, and integrators reproducing fills
off-chain must model the vAMM as a level ladder rather than a curve swap: `splitAcrossQuoters` and
`vammQuoteLevels` mirror the program's own math. `placeAndTakePerpOrderV1`, `placeAndMakePerpOrderV1`
and `fillPerpOrderV1` rest an unfilled restable remainder on the market's book instead of cancelling
it or leaving it in `User.orders`; the v0 instructions keep their pre-CLOB account lists. Signed-msg
orders carry a `network` tag and an optional signed route that binds the filler to it. A quoter
reports depth it could not reach (`withheldPrice` / `withheldBase`) and the router reserves it, so a
worse price cannot take what a book was standing on — immediate-or-cancel is exempt and pays for its
immediacy.

**The book.** A standalone CLOB program holds resting limit orders, reached through velocity
adapters that gate margin and unwind aggregates: place, cancel, cancel-all
(`CancelAllClobOrdersParams`), modify, and the keeper arms — `force_cancel_clob_orders`,
`crank_clob_evict`, `crank_clob_remove_expired`. Plain limits live only there. A trigger order
becomes a book order through `trigger_clob_order`, leaving a shadow in `User.orders` that frees on
fill, cull, expiry or cancel and re-arms on eviction. An activation-slot speed bump replaces JIT, so
the `jit-proxy` package and program are deleted.

**Quoters.** `QuoterV0` registers an external quoter per (market, program, user) with its CPI
surface, its routing tier and a maker-declared reprice watch; entries are born unapproved, the maker
keeps a kill switch, and the admin vets the surface. Velocity signs those CPIs as a dedicated
`["quoter_signer"]` PDA rather than the vault authority. The wire is `quote_v0`, `execute_v0` and the
optional `quote_l3_v0`, which reports the resting orders behind a ladder and the user each settles
against — a book implements it, and a quoter that fills from one account has its ladder attributed to
that account, so a reader never decodes a book. Responses are validated, not trusted: a `Custom`
entry may only move the account its registration consented for, a `Clob` entry may move any user the
transaction carries except the taker, executed volume is held to the router's allocation and its
notional to the prices quoted. The entry type is a warm-admin decision and a market names its book
once, permanently.

**Cranking.** Expiry, eviction, crossed books, crossed taker remainders, trigger arming and
liquidations all land with nobody submitting them: `ClobCrankConditionsV0` per market and
`UserConditionsV0` per user hold relay condition blocks and a keeper-payment reservoir, with
simulation-only resolvers staging each executor. `getUserConditionsPublicKey`,
`getClobCrankConditionsPublicKey` and `getRelayScratchPublicKey` replace the per-flow condition PDAs.
Cross cranks are permissionless and revert unless the spread clears both takers' fees and the
market's `min_cross_surplus` floor.

**Books off-chain.** `quoteRouter` answers what a taker of a given direction and size can get, per
source, in fill order, by simulating the real fill: one call returns verified books plus the orders
behind them and the users a fill has to carry. `RouterQuoteBufferV0` holds the answer, read out of
post-simulation state, and a market with more quoters than one transaction can carry is read in
passes.

**Also:** velocity's own events are versioned (`ProtocolUserWithdrawRecordV0`) with reserved tail
space on the new accounts, and the admin CLI gains the `quoter` and `clob-market` command groups
plus `fees withdraw-protocol-user`.
