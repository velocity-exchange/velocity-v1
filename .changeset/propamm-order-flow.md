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
`crank_clob_evict`, `crank_clob_remove_expired`. Velocity never reads the book's arena:
which order to remove, whether the book crosses itself, and what a set of refs still names
come back from the book's own instructions, its depth comes through the same `quote_v0`
every other source answers on, the rules an order has to satisfy come from `order_rules_v0`,
and the relay conditions that watch the book's state live on the book. Velocity knows the
CLOB's instruction wire and nothing about its account layout. One resolver,
`resolveClobCrank`, answers every one of a market's CLOB crank conditions — relay names the
condition that fired, so the three per-condition resolvers it replaces are gone from the IDL.
`placeClobOrder` and `modifyClobOrder` therefore take no crank-conditions account, and
`updatePerpMarketClobQuoter` takes the book's program and the quoter signer. Plain limits live
only there. A trigger order becomes a book order through `trigger_clob_order`, leaving a shadow in `User.orders` that frees on
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
market's `min_cross_surplus` floor. Each crank's keeper payment is *derived*, not set:
`StateAccount.transactionFeeRails` says what one transaction costs to land — an inclusion fee, a
per-signature fee and a rate on the cost units a transaction requests — and a market's attach prices
every crank from it and the cost units the admin measured for that crank. A book removal and a
two-legged cross differ by an order of magnitude in what they request, so one figure for the market
would either underpay the cross or overpay every removal. `math/crankFee` mirrors the on-chain
arithmetic (`requestedCostUnits`, `transactionCost`, `deriveCrankPayments`) and the runtime's cost
model, so a client can predict what an attach will write. Two cranks price themselves above that base, because a flat figure covers a quiet market
and nothing more: an expiry's offer climbs linearly with how long it went unclaimed (to 5,000
lamports over five minutes), and a liquidation repays the priority fee its keeper paid — for no
more compute units than the crank is measured to need, so a keeper is made whole without profiting
either by inflating its limit or by requesting less than it is repaid for — capped at
`StateAccount.liquidationCrankReimbursementBps` of the recovery and converted through
`StateAccount.solSpotMarketIndex`. A liquidation that fills nothing pays nothing. `AdminClient.updateLiquidationCrankReimbursement`
and CLI `fees set-liquidation-crank-reimbursement` set both; they default to zero, which
leaves the flat payment. `AdminClient.updateTransactionFeeRails`
and CLI `fees set-transaction-rails` re-price every crank in one write; markets take the new rate on
their next `quoter set-market-clob`, which now takes `--crank-cu` flags instead of a lamport figure.

**Transaction sizing.** The client asks for what it uses. `VelocityClient` defaults
`txParams.useSimulatedComputeUnits` to `true`, so a transaction's compute limit comes from
simulating it rather than from a flat 600,000; `txParams.computeUnits` becomes the ceiling that
clamps the simulated figure and the fallback when simulation fails, and
`useSimulatedComputeUnits: false` restores the old behaviour at the cost of one RPC round trip
saved. `txParams.loadedAccountsDataSize` is new and defaults to
`LOADED_ACCOUNTS_DATA_SIZE_DEFAULT` (12 MiB), emitted as a `SetLoadedAccountsDataSizeLimit`
instruction; 0 omits it and takes the network's 64 MiB default. Both limits are billed on what a
transaction *requests*, so asking for nothing in particular pays for room it never uses — and the
velocity program and its program data count toward the loaded-accounts limit, because the
transaction names the program. Raise it for a transaction that genuinely loads more. The
instruction is **appended**, and a client adding its own must append too: the runtime finds
compute-budget instructions by program id wherever they sit, but an instruction added at the front
shifts every index behind it, and `createMinimalEd25519VerifyIx` points at the instruction holding
the message it verifies by absolute index.

**Books off-chain.** `quoteRouter` answers what a taker of a given direction and size can get, per
source, in fill order, by simulating the real fill: one call returns verified books plus the orders
behind them and the users a fill has to carry. `RouterQuoteBufferV0` holds the answer, read out of
post-simulation state, and a market with more quoters than one transaction can carry is read in
passes.

**Also:** velocity's own events are versioned (`ProtocolUserWithdrawRecordV0`) with reserved tail
space on the new accounts, and the admin CLI gains the `quoter` and `clob-market` command groups
plus `fees withdraw-protocol-user`. `getRouteDigest(route)` mirrors the program's route digest —
the four bytes `Order.routeDigest` holds — which a filler needs because the program rejects a fill
whose claimed route does not digest to what the order carries.
