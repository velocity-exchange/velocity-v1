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
and `fillLegacyDlobOrder` rest an unfilled restable remainder on the market's book instead of cancelling
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
`updatePerpMarketClobQuoter` takes the book and its program. Plain limits live
only there. A trigger order becomes a book order through `trigger_limit_order_v1`, leaving a shadow in `User.orders` that frees on
fill, cull, expiry or cancel and re-arms on eviction. An activation-slot speed bump replaces JIT, so
the `jit-proxy` package and program are deleted.

**Quoters.** `QuoterV0` registers an external quoter per (market, program, user) with its CPI
surface, its routing tier and a maker-declared reprice watch; entries are born unapproved, the maker
keeps a kill switch, and the admin vets the surface. Velocity signs every external quoter CPI as
the market's quoter-slab PDA rather than the vault authority (see the registry section below). The wire is `quote_v0`, `execute_v0` and the
optional `quote_l3_v0`, which reports the resting orders behind a ladder and the user each settles
against — a book implements it, and a quoter that fills from one account has its ladder attributed to
that account, so a reader never decodes a book. Responses are validated, not trusted: a `Custom`
entry may only move the account its registration consented for, a `Clob` entry may move any user the
transaction carries except the taker, executed volume is held to the router's allocation and its
notional to the prices quoted. The entry type is a warm-admin decision and a market names its book once,
permanently: `PerpMarketAccount.clobMarket` stores the book account, written at registration,
and every accounts struct that names both binds them with `has_one`.

**Cranking.** Expiry, eviction, crossed books, crossed taker remainders, trigger arming and
liquidations all land with nobody submitting them: `ClobCrankConditionsV0` per market and
`UserConditionsV0` per user hold relay condition blocks and a keeper-payment reservoir, with
simulation-only resolvers staging each executor. `getUserConditionsPublicKey`,
`getClobCrankConditionsPublicKey` and `getRelayScratchPublicKey` replace the per-flow condition PDAs.

**Every user gets a `UserConditionsV0`.** `initializeUser` now _requires_ the `userConditions`
account rather than accepting `None`, and the payer funds its rent with the account. Relay can
only watch an account that exists, and the moment coverage matters is the moment somebody else's
transaction gave the user a position — a resting maker order filled by a keeper, or a
signed-message order submitted by a filler — where the user is not a signer and no rent can be
charged to them. Creating it up front is also what makes every later sync permissionless: the
account is already paid for, so anyone can keep it current. Vault velocity users are no
exception, so `initializeVault` and `initializeVaultWithProtocol` gain `velocityUserConditions`.

**Relay liquidation predicts nothing.** `UserConditionsV0` no longer stores per-exposure
liquidation thresholds; `LiqSlotMetaV0` and the `slots` array are gone and the account is 5,512
bytes rather than 7,864. Solving, per position, the price at which an account turns liquidatable
meant a second implementation of the margin engine beside the real one — approximate by
construction, needing to track every future change to margin — to buy latency a keeper bot
already provides. Instead a `LIQ_LIVENESS_POLL` condition wakes on a clock and the resolver runs
`calculate_margin_requirement_and_total_collateral_and_liability_info`, the same code the
executor runs, reporting no work when the account is healthy. What the account carries for
liquidation is the margin map that calculation needs, and the watch and fallback keep it current.
Spot-only distress stays a keeper-bot path: `liquidate_spot` settles by handing the liquidator the
borrow and the collateral behind it, so a protocol keeper would take on inventory it has no venue
to unwind.
Cross cranks are permissionless and revert unless the spread clears both takers' fees and the
market's `min_cross_surplus` floor. Each crank's keeper payment is _derived_, not set:
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
leaves the flat payment.

Every reservoir is funded from one place. `CrankTreasuryV0`
(`getCrankTreasuryPublicKey`) is the protocol's single crank pool: markets refill themselves
from it through the permissionless `refillCrankReservoir` crank, and a user's liquidation-
conditions resync is paid from it too, so no per-market or per-user balance is watched or
topped up by hand. A reservoir mirrors its spendable lamports into its own account data
(`ClobCrankConditionsV0.spendableMirror`) because a relay watch reads data and a lamport
balance is metadata; a condition on that value wakes the refill. Cranks are still _paid_ by
the market reservoir they crank rather than from the treasury directly — a crank writes
whatever pays it, and a writable account has a fixed compute budget per block, so one payer
for every crank would serialize the protocol's cranks into that budget exactly when a
market-wide move needs them landing in parallel. `AdminClient.initializeCrankTreasury`,
`updateCrankTreasury`, `withdrawCrankTreasury` and `sweepCrankReservoir` (CLI
`fees init-crank-treasury`, `set-crank-treasury`, `withdraw-crank-treasury`,
`sweep-crank-reservoir`) create, configure and drain it; funding it is a plain SOL transfer,
and the sweep moves lamports back out of a retired or over-provisioned market's reservoir so
they do not travel one way only. `clob-market init` therefore drops `--fund-reservoir`.

A reservoir is held between two levels, both counted in cranks rather than lamports so one
setting fits every market: `refillTargetCranks` is how full a refill leaves it, and
`refillWatermarkCranks` is when a refill wakes. The watermark must cover the refill's own
round trip — a turner polls, simulates and lands it while the reservoir keeps paying — and a
market-wide move is when cranks fire fastest and the network is slowest to land one. The
target is read at refill time and reaches every market at once; the watermark is resolved to
lamports at attach and stored on the market (`ClobCrankConditionsV0.refillWatermarkLamports`),
because it is the threshold that market's wake condition carries, so it reaches a market on
its next attach. What a refill _pays_ is priced from the rails like every other crank and
stored on the market it fills (`CrankPaymentsV0.refill`, `--crank-cu-refill`), because a
condition has to advertise a floor a turner can filter on.

Opting a user into self-maintaining liquidation conditions states what its resync pays, and
the treasury is now the payer, so that figure is capped (`LIQ_SYNC_MAX_COST_UNITS`) and drawn
at most once per `syncFallbackSlots` (`UserConditionsV0.lastPaidSyncSlot`) — opting in is
permissionless, and neither bound existed while the account paid for itself. `AdminClient.updateTransactionFeeRails`
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
transaction _requests_, so asking for nothing in particular pays for room it never uses — and the
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
plus `fees withdraw-protocol-user`. `clob-market update-config` retunes a live book's mutable
config, including `--reservation-grace-slots`, which bounds how long a taker remainder's claim on
the depth it crosses is honoured. `getRouteDigest(route)` mirrors the program's route digest —
eight bytes on `SignedMsgUserOrders` — which a filler needs because the program rejects a fill
whose claimed route does not digest to what the order carries.

The quoter registry's approved set moves into one `QuoterSlabV0` account per market.

**Why.** A router fill spent one account lock per quoter on its `QuoterV0` entry alone, against
the 64 a transaction can name. The slab holds every approved config in one account, so a quoter
costs two unshared locks (its program and its response account) plus the one slab every quoter
shares.

**The registry is now two halves.** `QuoterV0` is the _staging_ entry: the maker's proposal, and
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

**Stored bindings.** `PerpMarketAccount.clobQuoter` is replaced by `clobMarket` — the market
stores its book account directly (designated once at registration) — and gains `quoterSlab`;
the slab header records its book (`QuoterSlabV0Account.clobMarket`). The program binds market,
slab, and book with `has_one` on every accounts struct that names them, so a wrong account
fails at the accounts layer. `updateQuoterApproved` gains a read-only `perpMarket` account.

**Approval records the program's deploy slot.** `updateQuoterApproved` takes the entry's
`quoterProgram` and an optional `quoterProgramData`, and stores the slot the program was last
deployed at in the new `QuoterV0.approvedProgramSlot`. An upgrade moves that slot, so a reader can
see the code changed instead of inferring it from behaviour. Nothing on chain checks the slot during
a fill, because that would cost an account lock per quoter.

Approval deliberately does _not_ require or impose a frozen program. A maker may upgrade. A `Custom`
entry can move only its own registered user, at a price held to its own quote and the taker's limit,
sized inside its own margin — so an upgrade can lose the maker's money and cannot take anyone
else's.

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
`fill_legacy_dlob_order` require that the transaction was full and that every loaded
user did something. Three new errors say which rule failed:
`FillerOmittedReachableMaker` (6395), `FillerPaddedTheUserSet` (6396), and
`FillerObligationUncountable` (6397).

Both fill instructions gain an optional `instructions_sysvar` account. A fill
needs it only to be counted, so a taker filling its own order can omit it — but
a filler that omits it is refused whenever a book withholds. The SDK and
`velocity-rs` builders always pass it.

**A quoter must deliver every unit it won.** A quoter that returned nothing for an
allocation it won was skipped, so the size went nowhere and a source that would
have filled it never saw it. Returning less than the allocation already failed;
returning nothing now fails the same way, with `QuoterFilledShort` (6398).

**A book's report is held to what velocity reserved.** Every CLOB order reserves `openBids` /
`openAsks` and an open-order slot on its owner's `PerpPosition` at placement, under that owner's
signature. Nothing outside velocity can write those, so they are now the bound on every response a
quoter returns: fills, sub-min culls, retired-order counts, the evict / expire removal cranks, and
both cross cranks fail with `QuoterReportExceedsReservation` (6402) when the report exceeds the
reservation, instead of saturating at zero.

The ceiling on what a compromised book can open for a user the transaction carries drops from that
user's free collateral to the size they actually posted, on the side they posted it. The exits an
owner signs — `cancelClobOrder`, `cancelAllClobOrders`, `forceCancelClobOrders` — still clamp and
log rather than fail, so a maker can always pull orders off a book that reports garbage.

**A Custom quoter can tighten its own oracle band.** New `QuoterV0Account.maxOracleDeviationBps`
(MARGIN_PRECISION units, so one unit is one basis point; `0` = no declaration) and
`updateQuoterMaxOracleDeviation`, signed by the entry authority. Velocity already bounds every
external leg by the market's `marginRatioInitial`; this asks for a tighter one, so a maker caps what
its own program can lose if that program is compromised.

It applies as the smaller of the declaration and the market's, so no value it can hold is wider than
the one the admin vetted — which is why it writes through to the approved slab copy without
re-vetting. A declared band also trims the entry's quoted ladder before the split, so an over-wide
quote costs that maker allocation rather than failing a fill that carries other makers.

`QuoterV0` is 792 bytes. New `math/router` exports `quoterOracleBand`,
`makerPriceBreachesOracleBand`, and `isReportWithinReservation` mirror the three predicates a client
needs to tell whether a fill will be accepted. Admin CLI: `velocity-admin quoter set-oracle-band`.

CLOB order surface: a client can place, cancel, modify and sweep resting book orders, and read back
what it is resting.

`VelocityClient` gains `placeClobOrder`, `cancelClobOrder`, `modifyClobOrder` and
`cancelAllClobOrders` with their `get*Ix` builders. A caller names a market and nothing else — the
book's account, its program and the PDAs resolve from the market's quoter slab.

An order's id is minted from `User.next_order_id`, the same counter the account's DLOB orders draw
from, so a client names an order the same way wherever it rests. `placeClobOrder` returns the
order's `ClobOrderRefV0`, which is what a cancel or a modify takes; the book verifies it against the
order id and fails closed on a stale one. A modify keeps the id and loses the queue position.

`rejectIfCrossed` refuses a placement that would rest crossed with the opposite side — what a
post-only order asks for. It is not what makes the order a maker: a resting CLOB order settles at
its own price on the maker fee schedule either way.

`UserClobOrdersClient` reads a user's resting book orders from the dlob-server, over
`GET /userOrders` or the `user_orders` websocket channel. Book orders have no `User.orders` slot, so
this is what replaces `user.getOpenOrders()` for them; every row carries the order's handle.

`liquiditySource` — and so `L2Level['sources']` — gains `'clob'` and `'propamm'`, the two sources
the book publisher adds to each price level.

Maker priority: on a book with an activation speed bump, only attested flow fills synchronously.

With a nonzero `default_activation_delay_slots`, a transaction not co-signed by the flow
authority cannot fill against the book in the same transaction. `placeAndTakePerpOrder` (v1)
and signed-message orders rest the whole order taker-origin through the default window instead,
and the cross cranks fill it; an IOC or a success condition on such a take is refused with
`UnattestedSynchronousTake` (6403). Keeper fills still run but the book quotes them no depth, so
they reach the vAMM and DLOB makers only. Cancels are never delayed, so a maker can always
reprice ahead of unattested aggression. Books with a zero default delay are unaffected.

Attestation has two transports: `placeAndTakePerpOrderV1`, `placeAndMakePerpOrderV1` and
`modifyOrderV1` carry an optional `flowAuthority` signer account, and
`placeSignedMsgTakerOrder` gains a `flowAttestation` argument — swift's detached signature over
the order's own signature plus an expiry, verified in-program. The flow authority never signs a
keeper-built transaction, and an attested fill pays no second signature fee.

The quoter wire carries the verdict: `QuoteArgsV0`/`ExecuteArgsV0` gain `taker_served_window`,
set by velocity for attested flow, and for the protocol cranks only when the orders they settle
rested at least two slots — a zero-delay book cannot launder fresh flow into the flag by
place-then-crank. The midpoint's
`require_attested_flow` checks that flag instead of the instructions sysvar, and its
`quote_v0`/`execute_v0` account lists drop the sysvar and velocity-State accounts — midpoint
quoter entries must be re-registered with the shorter legs.

`quoteRouter` takes the same fact as an argument (`taker_served_window`): a view for unprotected
flow shows no depth from a bumped book or a protected quoter, matching the fill's route.

`QuoterV0Account` gains `bookTickSize`, `bookMinOrderSize` and
`bookDefaultActivationDelaySlots` — the book's placement rules, mirrored onto the entry by
`updatePerpMarketClobQuoter` so the fill and placement paths stop CPI'ing `order_rules_v0`
entirely — the attach is the one remaining reader. Re-run the attach
after changing a book's rules; its `quoter` account is now writable.

Signed-message orders route at placement and rest on the CLOB

A signed-message order is routed when it is placed, and whatever it cannot fill rests on the
market's book as a taker-origin remainder instead of on the DLOB. The activation-slot auction then
decides who fills it on price rather than on who lands a transaction first.
`place_and_make_signed_msg_perp_order` is removed: it existed only to match a signed-message order
already resting in `User.orders`.

The route digest moves off `Order` onto `SignedMsgUserOrders`, next to the CLOB order id the
remainder rests under, and widens from five bytes to eight. `getRouteDigest` mirrors the new width.
`Order.route_digest` is retired to padding, so `Order` stays 104 bytes.

A fired trigger rests taker-origin too. It came to trade, so a cross settles at the counterparty's
price rather than picking it off at its own. Its owner cannot cancel it inside the activation
window; liquidation force-cancel stays exempt and `max_ts` still bounds its life.

A resting remainder claims the depth it crosses, and claimed depth is withheld from every book
read — `quote_v0`, `quote_l3_v0`, `execute_v0` and `next_cross_v0` — so a client sees less depth
than the orders on the book suggest, and a take can fill less than a raw order listing implies.
The claim is what stops the remainder being frontrun: without it a taker buys the ask the
remainder crosses and reposts it worse, and the remainder pays the worse price.

Two consequences to design around. A claim outranks price, so an ordinary order priced better on
the claimant's own side still takes nothing from claimed depth, and with a maker resting in front
of a remainder both cross cranks go quiet until the claim lapses — bounded by
`reservation_grace_slots`, which an admin retunes. And attestation buys a synchronous fill
against unclaimed depth only: an attested take that reaches nothing rests as a remainder of its
own rather than lifting the cover another remainder is waiting on.

`crank_cross_match` now takes `{ market_index, size }` and runs as two ordinary router fills, so
it costs about 328,000 compute units rather than 75,000 — past one instruction's 200,000 default,
so a keeper must request a budget for it.

A partly filled remainder keeps its order id and its queue position. The book gained `fill_v0`, so
velocity reports the base it settled and the order shrinks in place rather than being cancelled and
re-placed at the back of its price level.

`L3RowV0` carries `node_index` and `placed_slot` and is 72 bytes rather than 64.

Fire a DLOB stop-market straight to the book with `triggerMarketOrderV1`.

`VelocityClient` gains `triggerMarketOrderV1` and its `getTriggerMarketOrderV1Ix` builder, plus
`VelocityCore.buildTriggerMarketOrderV1Instruction`. Unlike `triggerOrder`, which flips a resting trigger
live and leaves it for a later fill crank, `triggerMarketOrderV1` fires a trigger-market order and fills
it against the book in the same instruction, resting only the remainder as a taker-origin order.
Nothing lingers live in `User.orders`. For DLOB trigger-market orders; a trigger-limit keeps its own
`triggerLimitOrderV1` path.

`PerpPosition` gains `reduceOnlyClobOrders`: the count of reduce-only orders the owner has resting on
the CLOB. The router caps a user's reduce-only fills to the position they reduce only while this is
non-zero, so a reduce-only stop can rest its remainder on the book without over-filling.

Limit orders no longer accept an oracle price offset.

The program refuses any `OrderType.LIMIT` order whose `oraclePriceOffset` is nonzero with
`InvalidOrderOracleOffset` (6055). An oracle-floating limit cannot rest on a CLOB, so such an
order could only strand in `User.orders` on the legacy DLOB. Use a PropAMM quoter for an
oracle-relative maker quote, or a repriced fixed-price limit. `OrderType.ORACLE` market orders
keep their oracle-relative auctions; the `Order.oraclePriceOffset` field and the order params
shape are unchanged.

Three CLOB-era instructions are renamed before first release: `fillPerpOrderV1` is now
`fillLegacyDlobOrder` (it fills only orders the legacy endpoints created, and is deleted with
them), `triggerOrderV1` is now `triggerMarketOrderV1`, and `triggerClobOrder` is now
`triggerLimitOrderV1` (the pair partitions trigger orders by type: a fired limit rests whole, a
fired market fills first). Client methods and instruction builders follow the new names.

The CLOB placement and taker routes drop their optional `crankConditions` account. The book
hosts its own crank wakes, so `placeAndTakePerpOrderV1`, `placeAndMakePerpOrderV1`,
`cancelOrdersV1`, `fillLegacyDlobOrder` and `placeSignedMsgTakerOrder` never read it; the
`clobAccounts` parameter shapes lose the field.

Liquidations and the funding mark TWAP read the market's book.

`liquidatePerpWithFill` fills its forced order through the router, so a liquidation reaches the
market's CLOB and its PropAMM quoters rather than only the makers the caller passes.
`getLiquidatePerpWithFillIx` appends the market's quoter section to the remaining accounts, and
takes an `extraQuoterAccounts` argument for further quoters. A market that names a book refuses
the call without that section, and book depth is reachable only for owners the transaction
carries, so pass the book's resting owners in `makerInfos`.

`updatePerpBidAskTwap` takes three optional accounts — `quoterSlab`, `clobMarket` and
`clobProgram` — and estimates each side of the market from the book merged with the `User`
accounts the caller passes. `getUpdatePerpBidAskTwapIx` resolves those three from the market, so
a caller supplies only the makers. A market that names a book refuses the crank without them, a
suspended book moves no mark, and both sources still drop a quote that has not rested for
`BID_ASK_TWAP_MIN_QUOTE_REST`.
