---
'@velocity-exchange/sdk': minor
'@velocity-exchange/admin-cli': minor
---

PropAMM order flow. A perp order fills through one router that spans the vAMM, a standalone CLOB
book and external quoter programs. `docs/DRIFT-TO-VELOCITY.md` is the migration reference. This
note is the surface.

## Router fills

Every perp fill runs one router pass. Each source publishes discrete price levels. The split walks
priority tiers in ascending order, the vAMM first, then the book, then custom quoters, and divides
pro rata inside a tier. `PerpFulfillmentMethod` and AMM JIT are gone, and the `jit-proxy` package
and program are deleted. A client that reproduces a fill off chain must model the vAMM as a ladder
of levels rather than a curve swap. `splitAcrossQuoters` and `vammQuoteLevels` mirror the program's
math. `vammQuoteLevels` shades the vAMM ladder only for the depth a rival book offers, so a client
that prices a fill against rival books gets a different answer than before.

`RouterAllocation.scaledQuote` reports the sum of price times base before the division into quote
units. The program holds a fill to that scalar, so a client that predicts whether a fill is
accepted needs it.

A quoter reports depth it could not reach in `withheldPrice` and `withheldBase`. When a book
withholds depth and the taker did not sign, `fill_perp_order` and `fill_legacy_dlob_order` require
that the transaction was full and that every loaded user did something. `FillerOmittedReachableMaker`
(6396), `FillerPaddedTheUserSet` (6397) and `FillerObligationUncountable` (6398) say which rule
failed. Both fill instructions take an optional `instructions_sysvar` account. A fill needs it only
to be counted, so a taker filling its own order can omit it, but a filler that omits it is refused
whenever a book withholds. The SDK and `velocity-rs` builders always pass it.

A quoter must deliver every unit it won. A quoter that returns less than its allocation, or nothing
at all, fails with `QuoterFilledShort` (6399).

Every CLOB order reserves `openBids`, `openAsks` and an open-order slot on its owner's
`PerpPosition` at placement, under that owner's signature. Nothing outside velocity can write those,
so they bound every response a quoter returns. Fills, sub-min culls, retired-order counts, the evict
and expire cranks, and both cross cranks fail with `QuoterReportExceedsReservation` (6403) when the
report exceeds the reservation. The ceiling on what a compromised book can open for a user the
transaction carries is the size that user posted, on the side they posted it. The exits an owner
signs still clamp and log rather than fail, so a maker can always pull orders off a book that
reports garbage.

`math/router` exports `quoterOracleBand`, `makerPriceBreachesOracleBand` and
`isReportWithinReservation`, the three predicates a client needs to tell whether a fill is accepted.

## The book

A standalone CLOB program holds the resting limit orders, reached through velocity adapters that
gate margin and unwind aggregates. Velocity never reads the book's arena. Which order to remove,
whether the book crosses itself, and what a set of refs still names come back from the book's own
instructions. Its depth comes through the same `quote_v0` every other source answers on, and the
relay conditions that watch its state live on the book. Velocity knows the CLOB's instruction wire
and nothing about its account layout.

`VelocityClient` gains `cancelOrderV1`, `cancelOrdersV1` and `modifyOrderV1` with their `get*Ix`
builders. A caller names a market and nothing else, because the book's account, its program and the
PDAs resolve from the market's quoter slab. Those instructions take `quoterSlab`, `clobMarket` and
`clobProgram`, and `clobProgram` is pinned to velocity's CLOB program id. The keeper arms are
`force_cancel_clob_orders`, `crank_clob_evict` and `crank_clob_remove_expired`.

An order's id is minted from `User.next_order_id`, the same counter the account's DLOB orders draw
from, so a client names an order the same way wherever it rests. A placement returns the order's
`ClobOrderRefV0`, which is what a cancel or a modify takes. The book verifies the ref against the
order id and fails closed on a stale one. A modify keeps the id and loses the queue position. A
partly filled order keeps both: the book exposes `fill_v0`, so velocity reports the base it settled
and the order shrinks in place instead of being cancelled and re-placed at the back of its level.

`rejectIfCrossed` refuses a placement that would rest crossed with the opposite side, which is what
a post-only order asks for. It is not what makes the order a maker. A resting CLOB order settles at
its own price on the maker fee schedule either way.

`UserClobOrdersClient` reads a user's resting book orders from the dlob-server, over
`GET /userOrders` or the `user_orders` websocket channel. Book orders have no `User.orders` slot, so
this replaces `user.getOpenOrders()` for them. Every row carries the order's handle.

`liquiditySource`, and so `L2Level['sources']`, gains `'clob'` and `'propamm'`, the two sources the
book publisher adds to each price level. `L3RowV0` carries `node_index` and `placed_slot` and is 72
bytes. `ClobRestUnavailable` is a new error.

A limit order can no longer carry an oracle price offset. The program refuses any `OrderType.LIMIT`
order whose `oraclePriceOffset` is nonzero with `InvalidOrderOracleOffset` (6055). An
oracle-floating limit cannot rest on a CLOB, so such an order could only strand in `User.orders` on
the legacy DLOB. Use a PropAMM quoter for an oracle-relative maker quote, or a repriced fixed-price
limit. `OrderType.ORACLE` market orders keep their oracle-relative auctions, and the
`Order.oraclePriceOffset` field and the order params shape are unchanged.

## Quoters and the slab

`QuoterV0` registers an external quoter per market, program and user, with its CPI surface, its
routing tier and a maker-declared reprice watch. It is 792 bytes. The entry is the staging half of
the registry: the maker's proposal, and the quoter's identity that signed routes,
`PerpMarket.clobQuoter` and relay conditions name. Nothing fills from it. `updateQuoterApproved`
copies the staged config into a slot on the market's `QuoterSlabV0`, and fills read only that copy,
so a staging edit stays inert until the admin copies again and the vetted copy keeps serving
meanwhile. Approval is presence in the slab. Slot 0 holds the market's book and slots 1 and up hold
`Custom` quoters. Revoking a `Custom` slot clears it. Revoking the book suspends it, so cancel and
removal paths keep working on a dead book. `updateQuoterActive`, `updateQuoterPriority` and
`updateQuoterMaxOracleDeviation` write through to the live slot when the slab is passed.

One slab holds every approved config for a market, so a fill spends two unshared account locks per
quoter, its program and its response account, plus the one slab every quoter shares. Router fills
and `quoteRouter` carry the slab in the account tail, and a slot is consulted when its response
account rides the transaction.

The market's slab PDA is the one identity velocity signs every external quoter CPI as: a book's
`place_authority`, a midpoint instance's `execute_authority`, and the signer slot in every
registered CPI account list.

Client surface: permissionless `initializeQuoterSlab({ marketIndex })` creates a one-slot slab, and
approval right-sizes the account from then on, so there is no extend instruction. The admin pays for
growth and a revocation refunds trailing vacancy. New `getQuoterSlabPublicKey` (seeds
`["quoter_slab", marketIndex]`), `decodeQuoterSlab` (the slot region is raw bytes past the header,
which the generated coder cannot read) and `VelocityClient.getQuoterSlabAccount`. Type mirrors:
`QuoterConfigV0`, `QuoterSlotV0`, `QuoterSlabV0Account`. The slab header records its own bump and
its book.

The wire is `quote_v0`, `execute_v0` and the optional `quote_l3_v0`, which reports the resting orders
behind a ladder and the user each settles against. A book implements it, and a quoter that fills from
one account has its ladder attributed to that account, so a reader never decodes a book. Responses
are validated, not trusted. A `Custom` entry may move only the account its registration consented
for. A `Clob` entry may move any user the transaction carries except the taker. Executed volume is
held to the router's allocation and its notional to the prices quoted.

`updateQuoterAccounts` takes `{ metas, quoteIndexes, executeIndexes }` in one call: one 12-slot
account list plus per-leg index lists.

A `Custom` quoter can tighten its own oracle band. `QuoterV0Account.maxOracleDeviationBps` is in
MARGIN_PRECISION units, so one unit is one basis point and `0` declares nothing.
`updateQuoterMaxOracleDeviation` is signed by the entry authority. Velocity already bounds every
external leg by the market's `marginRatioInitial`, and this asks for a tighter one, so a maker caps
what its own program can lose if that program is compromised. It applies as the smaller of the
declaration and the market's, so no value it can hold is wider than the one the admin vetted, which
is why it writes through to the approved slab copy without re-vetting. A declared band also trims the
entry's quoted ladder before the split, so an over-wide quote costs that maker allocation rather than
failing a fill that carries other makers. Admin CLI: `velocity-admin quoter set-oracle-band`.

`updateQuoterApproved` takes the entry's `quoterProgram` and an optional `quoterProgramData`, and
stores the slot the program was last deployed at in `QuoterV0.approvedProgramSlot`. An upgrade moves
that slot, so a reader can see the code changed instead of inferring it from behaviour. Nothing on
chain checks the slot during a fill, because that would cost an account lock per quoter. Approval
does not require or impose a frozen program. A maker may upgrade. A `Custom` entry can move only its
own registered user, at a price held to its own quote and the taker's limit, sized inside its own
margin, so an upgrade can lose the maker's money and cannot take anyone else's.

The registry's account rules: `initializeQuoter` takes a quoter slab, required when the entry is a
book, so a market's slab exists before its book is registered. `updateQuoterApproved` takes the book
the market designated, required when the entry is a book. `updateQuoterActive`, `updateQuoterConfig`,
`updateQuoterAccounts` and `updateQuoterWatch` take the state account, because a book entry answers
to the protocol's warm admin instead of the key that registered it. A quoter a maker owns still
answers only to the key that created it. A registered account list may not name the market's
designated book.

`PerpMarketAccount.clobMarket` stores the book account, written once at registration, and
`PerpMarketAccount.quoterSlab` stores the slab. The program binds market, slab and book with
`has_one` on every accounts struct that names them, so a wrong account fails at the accounts layer.
`QuoterV0Account` also carries `bookTickSize`, `bookMinOrderSize` and
`bookDefaultActivationDelaySlots`, the book's placement rules, mirrored onto the entry by
`updatePerpMarketClobQuoter` so the fill and placement paths never CPI `order_rules_v0`. The attach
is the one remaining reader, and its `quoter` account is writable, so re-run it after changing a
book's rules. `QuoterSlabFull` (6405) and `QuoterNotOnSlab` (6406) are new errors. Attaching a CLOB
to a market requires a `min_cross_surplus` above zero.

## Attested flow and the activation delay

An activation-slot speed bump replaces JIT. On a book with a nonzero `default_activation_delay_slots`,
a transaction not co-signed by the flow authority cannot fill against the book in the same
transaction. `placeAndTakePerpOrder` (v1) and signed-message orders rest the whole order
taker-origin through the default window instead, and the cross cranks fill it. An immediate-or-cancel
order or a success condition on such a take is refused with `UnattestedSynchronousTake` (6404).
Keeper fills still run, but the book quotes them no depth, so they reach the vAMM and DLOB makers
only. Cancels are never delayed, so a maker can always reprice ahead of unattested aggression. A book
with a zero default delay is unaffected.

Attestation has two transports. `placeAndTakePerpOrderV1`, `placeAndMakePerpOrderV1` and
`modifyOrderV1` carry an optional `flowAuthority` signer account, and `placeSignedMsgTakerOrder`
takes a `flowAttestation` argument, which is swift's detached signature over the order's own
signature plus an expiry, verified in-program. The flow authority never signs a keeper-built
transaction, and an attested fill pays no second signature fee.

The quoter wire carries the verdict. `QuoteArgsV0` and `ExecuteArgsV0` carry `taker_served_window`,
which velocity sets for attested flow, and for the protocol cranks only when the orders they settle
rested at least two slots, so a zero-delay book cannot launder fresh flow into the flag by placing
and then cranking. The midpoint's `require_attested_flow` checks that flag instead of the
instructions sysvar, and its `quote_v0` and `execute_v0` account lists carry neither the sysvar nor
the velocity State account. `quoteRouter` takes `taker_served_window` as an argument, so a view for
unprotected flow shows no depth from a bumped book or a protected quoter, matching the fill's route.

## Signed-message orders

A signed-message order is routed when it is placed, and whatever it cannot fill rests on the market's
book as a taker-origin remainder rather than on the DLOB. The activation-slot auction then decides
who fills it on price rather than on who lands a transaction first.
`place_and_make_signed_msg_perp_order` is removed, because it existed only to match a signed-message
order already resting in `User.orders`.

The route digest is eight bytes on `SignedMsgUserOrders`, next to the CLOB order id the remainder
rests under. `getRouteDigest(route)` mirrors it, which a filler needs because the program rejects a
fill whose claimed route does not digest to what the order carries.

The network tag is required. A message that names no cluster is refused the same way a message naming
the wrong cluster is, because both replay the same way. `network` is a required field on both message
types, `signedMsgNetworkForEnv` is exported, and the client stamps the tag from its configured `env`,
so a caller that signs through the client states nothing.
`SignedMsgOrderParamsMessageInput` and `SignedMsgOrderParamsDelegateMessageInput` keep the field
optional at that edge. `VelocityClient.env` defaults to `mainnet-beta`, so an integrator that runs on
devnet without setting `env` signs a tag devnet refuses.

## Triggers and remainders

A trigger order becomes a book order through `trigger_limit_order_v1`, leaving a shadow in
`User.orders` that frees on fill, cull, expiry or cancel and re-arms on eviction.
`VelocityClient.triggerMarketOrderV1` and `getTriggerMarketOrderV1Ix`, plus
`VelocityCore.buildTriggerMarketOrderV1Instruction`, fire a DLOB trigger-market order and fill it
against the book in the same instruction, resting only the remainder as a taker-origin order. Nothing
lingers live in `User.orders`. `triggerOrder` still flips a resting trigger live and leaves it for a
later fill crank.

A fired trigger rests taker-origin. It came to trade, so a cross settles at the counterparty's price
rather than picking it off at its own. Its owner cannot cancel it inside the activation window.
Liquidation force-cancel stays exempt and `max_ts` still bounds its life.

`PerpPosition.reduceOnlyClobOrders` counts the reduce-only orders the owner has resting on the CLOB.
While it is nonzero the router caps that user's reduce-only fills to the position they reduce, so a
reduce-only stop can rest its remainder on the book without over-filling.

A resting remainder claims the depth it crosses, and claimed depth is withheld from every book read,
`quote_v0`, `quote_l3_v0`, `execute_v0` and `next_cross_v0`. A client therefore sees less depth than
the orders on the book suggest, and a take can fill less than a raw order listing implies. The claim
is what stops the remainder being frontrun: without it a taker buys the ask the remainder crosses and
reposts it worse, and the remainder pays the worse price.

Two consequences to design around. A claim outranks price, so an ordinary order priced better on the
claimant's own side still takes nothing from claimed depth, and with a maker resting in front of a
remainder both cross cranks go quiet until the claim lapses. `reservation_grace_slots` bounds that
window, and `clob-market update-config` retunes it on a live book through
`--reservation-grace-slots`. Attestation also buys a synchronous fill against unclaimed depth only:
an attested take that reaches nothing rests as a remainder of its own rather than lifting the cover
another remainder waits on.

`crank_cross_match` takes `{ market_index, size }` and runs as two ordinary router fills, so it costs
about 328,000 compute units rather than 75,000. That is past one instruction's 200,000 default, so a
keeper must request a budget for it. Cross cranks are permissionless and revert unless the spread
clears both takers' fees and the market's `min_cross_surplus` floor.

## Relay cranks and their funding

Expiry, eviction, crossed books, crossed taker remainders, trigger arming and liquidations all land
with nobody submitting them. `ClobCrankConditionsV0` per market and `UserConditionsV0` per user hold
relay condition blocks and a keeper-payment reservoir, with simulation-only resolvers staging each
executor. `getUserConditionsPublicKey`, `getClobCrankConditionsPublicKey` and
`getRelayScratchPublicKey` are the PDA helpers. One resolver, `resolveClobCrank`, answers every one
of a market's CLOB crank conditions, because relay names the condition that fired.

Every user gets a `UserConditionsV0`. `initializeUser` requires the `userConditions` account rather
than accepting `None`, and the payer funds its rent with the account. Relay can only watch an account
that exists, and the moment coverage matters is the moment somebody else's transaction gave the user
a position, such as a resting maker order filled by a keeper or a signed-message order submitted by a
filler, where the user is not a signer and no rent can be charged to them. Creating it up front is
also what makes every later sync permissionless, because the account is already paid for, so anyone
can keep it current. Vault velocity users are no exception, so `initializeVault` and
`initializeVaultWithProtocol` take `velocityUserConditions`.

Relay liquidation predicts nothing. `UserConditionsV0` stores no per-exposure liquidation thresholds.
Solving, per position, the price at which an account turns liquidatable means a second implementation
of the margin engine beside the real one, approximate by construction and needing to track every
future change to margin, to buy latency a keeper bot already provides. Instead a `LIQ_LIVENESS_POLL`
condition wakes on a clock and the resolver runs
`calculate_margin_requirement_and_total_collateral_and_liability_info`, the same code the executor
runs, and reports no work when the account is healthy. What the account carries for liquidation is
the margin map that calculation needs, and the watch and fallback keep it current. Spot-only distress
stays a keeper-bot path, because `liquidate_spot` settles by handing the liquidator the borrow and
the collateral behind it, so a protocol keeper would take on inventory it has no venue to unwind.

Each crank's keeper payment is derived, not set. `StateAccount.transactionFeeRails` says what one
transaction costs to land, an inclusion fee, a per-signature fee and a rate on the cost units a
transaction requests. A market's attach prices every crank from those rails and the cost units the
admin measured for that crank. A book removal and a two-legged cross differ by an order of magnitude
in what they request, so one figure for the market would either underpay the cross or overpay every
removal. `math/crankFee` mirrors the on-chain arithmetic (`requestedCostUnits`, `transactionCost`,
`deriveCrankPayments`) and the runtime's cost model, so a client can predict what an attach writes.

Two cranks price themselves above that base, because a flat figure covers a quiet market and nothing
more. An expiry's offer climbs linearly with how long it went unclaimed, to 5,000 lamports over five
minutes. A liquidation repays the priority fee its keeper paid, for no more compute units than the
crank is measured to need, so a keeper is made whole without profiting either by inflating its limit
or by requesting less than it is repaid for. That repayment is capped at
`StateAccount.liquidationCrankReimbursementBps` of the recovery and converted through
`StateAccount.solSpotMarketIndex`. A liquidation that fills nothing pays nothing.
`AdminClient.updateLiquidationCrankReimbursement` and CLI
`fees set-liquidation-crank-reimbursement` set both. They default to zero, which leaves the flat
payment.

Every reservoir is funded from one place. `CrankTreasuryV0` (`getCrankTreasuryPublicKey`) is the
protocol's single crank pool. Markets refill themselves from it through the permissionless
`refillCrankReservoir` crank, and a user's liquidation-conditions resync is paid from it too, so no
per-market or per-user balance is watched or topped up by hand. A reservoir mirrors its spendable
lamports into its own account data (`ClobCrankConditionsV0.spendableMirror`), because a relay watch
reads data and a lamport balance is metadata, and a condition on that value wakes the refill. Cranks
are still paid by the market reservoir they crank rather than from the treasury directly. A crank
writes whatever pays it, and a writable account has a fixed compute budget per block, so one payer
for every crank would serialize the protocol's cranks into that budget exactly when a market-wide
move needs them landing in parallel. `AdminClient.initializeCrankTreasury`, `updateCrankTreasury`,
`withdrawCrankTreasury` and `sweepCrankReservoir` (CLI `fees init-crank-treasury`,
`set-crank-treasury`, `withdraw-crank-treasury`, `sweep-crank-reservoir`) create, configure and drain
it. Funding it is a plain SOL transfer, and the sweep moves lamports back out of a retired or
over-provisioned market's reservoir, so they do not travel one way only. `clob-market init` takes no
`--fund-reservoir`.

A reservoir is held between two levels, both counted in cranks rather than lamports, so one setting
fits every market. `refillTargetCranks` is how full a refill leaves it and `refillWatermarkCranks` is
when a refill wakes. The watermark must cover the refill's own round trip, because a turner polls,
simulates and lands it while the reservoir keeps paying, and a market-wide move is when cranks fire
fastest and the network is slowest to land one. The target is read at refill time and reaches every
market at once. The watermark is resolved to lamports at attach and stored on the market
(`ClobCrankConditionsV0.refillWatermarkLamports`), because it is the threshold that market's wake
condition carries, so it reaches a market on its next attach. What a refill pays is priced from the
rails like every other crank and stored on the market it fills (`CrankPaymentsV0.refill`,
`--crank-cu-refill`), because a condition has to advertise a floor a turner can filter on.

Opting a user into self-maintaining liquidation conditions states what its resync pays, and the
treasury is the payer, so that figure is capped (`LIQ_SYNC_MAX_COST_UNITS`) and drawn at most once
per `syncFallbackSlots` (`UserConditionsV0.lastPaidSyncSlot`). Opting in is permissionless.
`AdminClient.updateTransactionFeeRails` and CLI `fees set-transaction-rails` re-price every crank in
one write. Markets take the new rate on their next `quoter set-market-clob`, which takes `--crank-cu`
flags instead of a lamport figure.

## Transaction sizing

The client asks for what it uses. `VelocityClient` defaults `txParams.useSimulatedComputeUnits` to
`true`, so a transaction's compute limit comes from simulating it rather than from a flat 600,000.
`txParams.computeUnits` becomes the ceiling that clamps the simulated figure, and the fallback when
simulation fails. `useSimulatedComputeUnits: false` restores the old behaviour and saves one RPC
round trip. `txParams.loadedAccountsDataSize` is new and defaults to
`LOADED_ACCOUNTS_DATA_SIZE_DEFAULT` (12 MiB), emitted as a `SetLoadedAccountsDataSizeLimit`
instruction. A `0` omits the instruction and takes the network's 64 MiB default. Both limits are
billed on what a transaction requests, so asking for nothing in particular pays for room it never
uses. The velocity program and its program data count toward the loaded-accounts limit, because the
transaction names the program. Raise it for a transaction that genuinely loads more.

The instruction is appended, and a client adding its own must append too. The runtime finds
compute-budget instructions by program id wherever they sit, but an instruction added at the front
shifts every index behind it, and `createMinimalEd25519VerifyIx` points at the instruction holding
the message it verifies by absolute index.

## Reading the book off chain

`quoteRouter` answers what a taker of a given direction and size can get, per source, in fill order,
by simulating the real fill. One call returns verified books plus the orders behind them and the
users a fill has to carry. `RouterQuoteBufferV0` holds the answer, read out of post-simulation state.
A market with more quoters than one transaction can carry is read in passes.

## Liquidation and the mark TWAP

`liquidatePerpWithFill` fills its forced order through the router, so a liquidation reaches the
market's CLOB and its PropAMM quoters rather than only the makers the caller passes.
`getLiquidatePerpWithFillIx` appends the market's quoter section to the remaining accounts and takes
an `extraQuoterAccounts` argument for further quoters. A market that names a book refuses the call
without that section, and book depth is reachable only for owners the transaction carries, so pass
the book's resting owners in `makerInfos`.

`updatePerpBidAskTwap` takes `quoterSlab`, `clobMarket` and `clobProgram`, and estimates each side of
the market from the book merged with the `User` accounts the caller passes.
`getUpdatePerpBidAskTwapIx` resolves those three from the market, so a caller supplies only the
makers. A market that names a book refuses the crank without them, a suspended book moves no mark,
and both sources still drop a quote that has not rested for `BID_ASK_TWAP_MIN_QUOTE_REST`.

## Other surface changes

Velocity's own events are versioned. `ProtocolUserWithdrawRecordV0` and
`AcceleratedReferralStatusChangedRecordV0` carry their own discriminators, and the new accounts keep
reserved tail space.

New endpoints take a single args struct (`PlaceAndTakePerpOrderV1Args`, `TriggerMarketOrderV1Args`,
`UpdateQuoterApprovedArgs` and the rest). `fillLegacyDlobOrder` takes `marketIndex` first.

The admin CLI gains the `quoter` and `clob-market` command groups plus `fees withdraw-protocol-user`.
`clob-market update-config` retunes a live book's mutable config. The CLI creates a market's slab
before it registers that market's book, and passes the registry's accounts on register, approve,
set-active, set-config, set-accounts and set-watch.
