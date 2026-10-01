---
'@velocity-exchange/sdk': major
'@velocity-exchange/admin-cli': minor
'@velocity-exchange/vaults-sdk': minor
---

PropAMM order flow. A perp order fills through one router that spans the vAMM, a standalone CLOB
book and external quoter programs. `docs/DRIFT-TO-VELOCITY.md` is the migration reference. This
note is the surface.

## The DLOB is gone

Velocity's original matching venue rested limit orders in the on-chain `User.orders` array and let
a keeper pick a taker's counterparties by choosing which maker accounts to pass. That venue is
removed. Every live order is on the book, one fill path serves every market, and no caller names a
counterparty: the books do.

These instructions are deleted, and a transaction that names one fails as an unknown instruction:
`place_perp_order`, `place_orders`, `place_scale_orders`, `place_and_take_perp_order` (v0),
`fill_perp_order`, `fill_legacy_dlob_order`, `revert_fill`, `trigger_order`,
`resolve_trigger_order`, and `place_and_make_perp_order` (v0). `place_trigger_orders_v1` replaces
the arming half of `place_orders`. The v1 endpoints replace the rest.

`User.orders` now holds unfired conditionals only: armed triggers, the SL/TP sidecars a
signed-message order writes, and the shadow a triggered trigger-limit keeps while its live order
rests on the book. The cancel and modify endpoints are unchanged, so an order left over from the
old venue is inert but still cancellable, and `release_order_reservation` returns its
`open_bids`/`open_asks` correctly. There is no deadline to clear one, but each still reserves
margin until its owner does.

The SDK follows. `VelocityClient.placePerpOrder` is now `placeTriggerOrders`, which takes an array
and refuses any order type that is not `TriggerMarket` or `TriggerLimit`; the rename is deliberate,
so a call that meant to rest a maker order fails at the type checker rather than on chain.
`fillPerpOrder`, `revertFill`, `placeOrders` and `placeScaleOrders`, with their `get*Ix` builders,
are removed. `placeAndTakePerpOrder` takes the market's CLOB accounts as its second argument, or
reads them off the market when they are omitted. Rest a maker order with `placeAndMakePerpOrder`
and take with `placeAndTakePerpOrder`.

The `DLOB` class and its subscribers are removed with the venue they served. `DLOB`, `DLOBNode`,
`NodeList`, `DLOBSubscriber`, `OrderSubscriber`, `AuctionSubscriber` and `UserMap.getDLOB` are gone,
as are the vAMM ladder generators that fed them (`getVammL2Generator`, `createL2Levels`,
`mergeL2LevelGenerators`, `getL2GeneratorFromDLOBNodes`, `L2OrderBookGenerator` and the
top-of-book quote amounts). `L2Level`, `L2OrderBook`, `L3Level`, `L3OrderBook`, `groupL2` and
`uncrossL2` move from `dlob/orderBookLevels` to `orderBookLevels`; the package barrel re-exports
them from the same names, so an import from `@velocity-exchange/sdk` does not move. A level's
`sources` now reports `'vamm'`, `'clob'` or `'propamm'` — `'dlob'` and `'indicative'` are gone.

Read resting liquidity from the book instead — `UserClobOrdersClient` for a user's own orders, the
dlob-server's `/l2`, `/l3` and `/userOrders` for the market's, and `quoteRouter` for what a taker of
a given size would actually get across every source. `calculateEstimatedPerpEntryPrice` takes an
`L2OrderBook` in place of a `DLOB`, so a caller passes the `/l2` answer and gets a vAMM-only
estimate from an empty one. `SlotSource` moves to `slot/SlotSubscriber`.

`placeSignedMsgTakerOrder`, `getPlaceSignedMsgTakerPerpOrderIxs` and `buildSwiftDepositTx` gain a
trailing `makerInfo`, and `buildSwiftDepositTx` now returns the transaction it builds.
The placement fills in the same instruction, and a fill reaches only the users the transaction
carries, so a keeper passes the book's resting owners there.

## Router fills

Every perp fill runs one router pass. Each source publishes discrete price levels. The split walks
priority tiers in ascending order, the vAMM first, then the book, then custom quoters, and divides
pro rata inside a tier. `PerpFulfillmentMethod` and AMM JIT are gone, and the `jit-proxy` package
and program are deleted. A client that reproduces a fill off chain must model the vAMM as a ladder
of levels rather than a curve swap. `splitAcrossQuoters` and `vammQuoteLevels` mirror the program's
math. In `vammQuoteLevels` a last-look rung reprices only base that the rivals at its price also
fill in the same take, at most `min(D, total - reach(P) - R)` just before the curve reaches the
rival price `P`. `R` is the rival depth priced better than `P`, including depth at or inside the
vAMM top, so a rival the take does not reach shades nothing. The budget reads only the levels the
router split reads. The ladder's top is the swap's first
marginal on the spread reserves, and a limit short of it quotes nothing. The rest of the ladder is
the honest curve.

`vammQuoteLevels` caps its ladder at the per-fill reserve throttle, exported as
`calculateAmmAvailableLiquidity`. The cap is `baseAssetReserve / maxFillReserveFraction`, then at
most half the side's room to the hard reserve bound, then floored to the step size. It is much
tighter than the room to that bound: at the default fraction of 100 a client that quoted the room
would over-allocate the vAMM by more than an order of magnitude, in the split preview, the depth
chart and any vAMM-versus-maker routing.

`calculateMaxBaseAssetAmountFillable` is deprecated and now delegates to
`calculateAmmAvailableLiquidity`. It omitted the half-the-side cap the program applies, so it
reported up to twice the depth one fill can take. `calculateBaseAssetAmountForAmmToFulfill` reads it
and therefore returned the same inflated figure. Its argument order is unchanged.

`RouterAllocation.scaledQuote` reports the sum of price times base before the division into quote
units. The program holds a fill to that scalar, so a client that predicts whether a fill is
accepted needs it.

A maker that cranks a fill earns the filler reward on the vAMM slice only. On a CLOB or Custom
match leg, only a filler that is not that leg's maker earns it, so the reward does not come out of
the protocol's share of the taker fee.

A quoter reports depth it could not reach in `withheldPrice` and `withheldBase`. When a book
withholds depth and the taker did not sign the transaction, the fill requires that the transaction
was full and that every loaded user did something. A loaded user counts only as the taker, the
filler or the user a consulted quoter fills for, so a subaccount of the taker's referrer does not. `FillerOmittedReachableMaker` (6396),
`FillerPaddedTheUserSet` (6397) and `FillerObligationUncountable` (6398) say which rule failed. The
rule binds every path a keeper assembles, such as a signed-message placement or a trigger crank. It
does not bind `place_and_take_perp_order_v1`, where the taker signs and chose its own account list.
It does not bind a liquidation either. The program writes a liquidation order at the oracle price
less the liquidator fee, so the fill stops at the makers the transaction carries, and a later call
liquidates the rest. When a book withholds, a fill that carries a quoter outside its signed route
fails with `FillerCarriedUnroutedQuoter` (6400). That count reads only the slots that quoted, less
the market's own book. The keeper paths take an optional `instructions_sysvar` account. A fill
needs it only to be counted, so a taker filling its own order can omit it, but a keeper that omits
it is refused whenever a book withholds. The SDK and `velocity-rs` builders always pass it.

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
A book that rests a level outside its oracle band quotes nothing to a fill, and
`bookRestsOutsideOracleBand` mirrors that skip. `customLadderInRoom` mirrors the trim of a `Custom`
quoter's ladder: the ladder ends at its first level outside the band, the quoter's room cuts the
base that is left, and each level floors to the market step, with the ladder ending after the first
level it floors. It takes the market's `stepSize` as its last argument. The fill measures every maker band at the safe MM oracle price, and so does the
keeper reward tier of an external fill. The band helpers therefore take the MM oracle price that
`getMMOracleDataForPerpMarket` returns, not the raw oracle.

A post-only order takes no external book. In a `ReduceOnly` market every maker may only reduce, and
a fill that grows one fails with `QuoterReportExceedsReservation`. A maker that is under liquidation
or bankrupt gets no depth, and a fill settles no quoter change for one. A maker with an equity floor
gets no depth while the exchange oracle is not valid for a match fill, even for a fill that reduces.
While the oracle does not allow a match fill, the route asks no quoter to quote. When several
consulted entries settle for one user, they split that user's room. A fill refuses a book change
whose last completed order is reduce-only when the base through that order passes the maker's
reduce-only cover. A vAMM past its reserve bound fails the fill with `InvalidAmmForFillDetected`.

The three accounts a CLOB order instruction takes are the exported type `ClobAccounts`, and
`VelocityClient.getClobAccounts(marketIndex)` is public, so a caller resolves them from a market
index rather than from the slab. `getPlaceAndMakePerpOrderIx` and `getForceCancelClobOrdersIx` take
them optionally and resolve them when they are omitted, as `getPlaceAndTakePerpOrderIx` already did.

## The book

A standalone CLOB program holds the resting limit orders, reached through velocity adapters that
gate margin and unwind aggregates. Velocity never reads the book's arena. Which order to remove,
whether the book crosses itself, and what a set of refs still names come back from the book's own
instructions. Its depth comes through the same `quote_v0` every other source answers on, and the
relay conditions that watch its state live on the book. Velocity knows the CLOB's instruction wire
and nothing about its account layout.

The book's `quote_v0` and `execute_v0` walks end after the first order they fill in part, so the
ladder a quote publishes ends where the fill ends. A take that a quote budget or a reduce-only cover
cuts floors to the book's order step. The book passes over the whole of an order that a crossing
taker remainder claims in part. Every fill of an owner on the swept side draws down that owner's
reduce-only cover, whether the filled order is reduce-only or not. The walk finds each order's owner
by a binary search over the caller's user set. No count of passed-over orders ends a walk, and the
arena capacity bounds it instead.

`initialize_market_v0` and `resize_market_v0` refuse an arena over 1024 slots, which is 512 orders a
side, with the CLOB error `CapacityOverCeiling`. A book created larger keeps working, but the CLOB
refuses to resize it. `initialize_market_v0` and `update_market_v0` require `blocking_min_size` to
exceed `min_order_size`, and the CLOB fails such a config with `InvalidConfig`. Velocity holds an
attached book to a higher floor. Its `blocking_min_size` must be at least ten times the larger of
the book's and the market's minimum order size, so a few minimum-size orders cannot keep the book
out of every fill. The attach, a book config update and a change to the market's grid or minimum
check it, and fail with `InvalidQuoterConfig`.

A CLOB rest judges its risk with `is_new_order_risk_increasing`, the predicate every placement uses.
That predicate counts the orders the position already rests. A rest that adds risk needs initial
margin, and any other rest needs maintenance margin.

`modifyOrderV1` gates the replacement on the same risk test every other placement uses. The test
counts the `open_bids` and `open_asks` the account already holds, not the bare position, so a
replacement that fits inside the position still requires initial margin and clears the buffered
equity floor when other orders rest behind it. A modify that passed before may now fail for a
caller resting several orders against one position.

`VelocityClient` gains `cancelOrderV1`, `cancelOrdersV1` and `modifyOrderV1` with their `get*Ix`
builders. A caller names a market and nothing else, because the book's account, its program and the
PDAs resolve from the market's quoter slab. Those instructions take `quoterSlab`, `clobMarket` and
`clobProgram`, and `clobProgram` is pinned to velocity's CLOB program id. `cancelOrderV1` and
`modifyOrderV1` take a trailing `takerOrigin` flag, which a `UserClobOrder` row carries. With it
set, the instruction carries the signed-message record, so a cancelled or modified signed-message
remainder releases its entry. A modify of a taker remainder rests the replacement as an ordinary
maker order. `SignedMsgOrderId` gains `marketIndex`,
because each book numbers its own orders. The keeper arms are
`force_cancel_clob_orders`, `crank_clob_evict` and `crank_clob_remove_expired`. `force_cancel_clob_orders`
cancels during a full exchange halt but pays no keeper fee, so the halt means the same thing for
it as for its `User.orders` twin, which refuses outright. It takes no `fillerStats` account, and it
judges a reduce-only order at the size it can close. `cancelOrdersV1` leaves every taker-origin
remainder on the book and reports the sweep as not exhaustive, because the sweep cannot release a
remainder's signed-message entry. `cancelOrderV1` with `takerOrigin` set removes one. Eviction
refuses a side whose worst order is a taker remainder inside its claim, with `TakerOriginBound`, and
never moves to a better-priced order. The evict and expiry cranks charge no maker fee
under a full halt. They and `crank_taker_origin_cross` refuse a filler outside pool 0 when a reward
is due.

`crank_clob_cancel_outside_band({ market_index, order_ref })` is a permissionless keeper crank. It
cancels a CLOB order whose price breaches the maker oracle band, because the router drops a whole
book from a fill when one of its levels breaches the band. The crank judges the order at the safe MM
oracle price the fill uses, with the band of the book's quoter entry, and behind the oracle gates of
the crossed-book cranks. It takes an optional `solSpotMarket` after `crankConditions`. A
program-keeper cancel charges the maker `max(flat_filler_fee, payment value)` at the SOL 5-minute
TWAP, and pays reservoir lamports only while that TWAP is live. An order inside the band fails with
`ClobOrderInsideOracleBand` (6461). No relay condition wakes this crank, and the SDK has no wrapper
for it.

`initialize_quoter_cross_conditions` bounds `expireFallbackSlots` at 9,000 slots. The endpoint is
permissionless and re-prices in place. A longer interval delays cross discovery for the maker's
quotes, so only the maker's authority chooses the value. Any other payer must pass
`QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS` (1,500), and its re-attach keeps the interval the block
already holds.

An order's id is minted from `User.next_order_id`, the same counter an armed trigger draws from,
so a client names an order the same way wherever it rests. A placement returns the order's
`ClobOrderRefV0`, which is what a cancel or a modify takes. The book verifies the ref against the
order id and fails closed on a stale one. A modify keeps the id and loses the queue position. A
partly filled order keeps both: the book exposes `fill_v0`, so velocity reports the base it settled
and the order shrinks in place instead of being cancelled and re-placed at the back of its level.

`rejectIfCrossed` refuses a placement that would rest crossed with the opposite side, which is what
a post-only order asks for. It is not what makes the order a maker. A resting CLOB order settles at
its own price on the maker fee schedule either way.

`placeAndMakePerpOrder` places or fails. A placement that the book or the margin gate refuses is an
error rather than a success that placed nothing, and the error names the rule, such as
`InvalidOrderMinOrderSize`, `InvalidOrderLimitPrice` or `InsufficientCollateral`. An IOC maker order
fails with `InvalidOrderIOC`. A builder code fails with `InvalidOrder`, because no fill pays a
maker-side builder fee. A `userOrderId` that a live slot order holds fails with
`UserOrderIdAlreadyInUse`. A reduce-only maker order rests, clamped to the position, also on a
`ReduceOnly` market. `TryPostOnly` and `Slide` read the book's best opposite order as well as the
vAMM. A price outside the maker oracle band fails with `PriceBandsBreached`, measured as the
router measures it, so an order rests outside the band only after the oracle moves. A taker
remainder or a fired trigger-limit rests clamped just inside the band. The placement
emits `OrderActionRecord(Place)` beside its `OrderRecord`.

`modifyOrderV1` rounds the replacement price onto the market's tick as a placement does, a bid down
and an ask up. It holds the replacement to the market's step and minimum order size, to the
open-interest cap, to the maker oracle band, and to the vAMM post-only check when `rejectIfCrossed`
is set. It refuses a market in settlement.

`cancelOrder` and `cancelOrdersByIds` fail with `OrderDoesNotExist` for an id the account minted that
holds no open slot order, such as a book order id or an order already filled. An id the account has
not minted is still a no-op, and `cancelOrdersByIds` skips a placed trigger.

`UserClobOrdersClient` reads a user's resting book orders from the dlob-server, over
`GET /userOrders` or the `user_orders` websocket channel. Book orders have no `User.orders` slot, so
this replaces `user.getOpenOrders()` for them. Every row carries the order's handle.

`liquiditySource`, and so `L2Level['sources']`, gains `'clob'` and `'propamm'`, the two sources the
book publisher adds to each price level. `L3RowV0` carries `node_index` and `placed_slot` and is 72
bytes. `ClobRestUnavailable` is a new error.

A limit order can no longer carry an oracle price offset. The program refuses any `OrderType.LIMIT`
order whose `oraclePriceOffset` is nonzero with `InvalidOrderOracleOffset` (6055). An
oracle-floating limit cannot rest on a CLOB, so such an order could only strand in `User.orders` on
the old venue, where nothing would fill it. Use a PropAMM quoter for an oracle-relative maker
quote, or a repriced fixed-price limit. `OrderType.ORACLE` market orders keep their oracle-relative auctions, and the
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
approval right-sizes the account from then on, so there is no extend instruction. It takes the perp
market writable. A market created before `PerpMarket.quoterSlab` existed reads the default key
there, and the call stores the slab on it. Every order path loads the slab, so such a market takes
no order until its slab exists. The admin pays for
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

The registry's account rules: `initializeQuoter` takes a quoter slab and the book as `clobMarket`,
both required when the entry is a book, so a market's slab exists before its book is registered. The
book must be an account of the CLOB program whose `place_authority` is the market's quoter slab. `updateQuoterApproved` takes the book
the market designated, required when the entry is a book. `updateQuoterActive`, `updateQuoterConfig`,
`updateQuoterAccounts` and `updateQuoterWatch` take the state account, because a book entry answers
to the protocol's warm admin instead of the key that registered it. A quoter a maker owns still
answers only to the key that created it. A registered account list may not name the market's
designated book.

Approval refuses a registered list that marks any account writable other than the entry's response
account. Every approval requires `responseAccount`, bound to the entry's configured response
account. The quoter program must own it, and it must not be executable, because a route consults
every slot whose response account rides the fill. `UpdateQuoterApprovedArgs` carries
`stagedConfigHash`, the SHA-256 of the staged `QuoterConfigV0` bytes, and an approval fails when the
staged config has another hash, so a maker edit after the admin's review cannot reach the slab. A
revocation ignores it. A `Clob` approval requires the CLOB's own `quote_v0`, `execute_v0` and
`quote_l3_v0` discriminators (`quoter_spec::discriminator`).
A midpoint approval also reads the instance. The instance's `execute_authority` must be the market's quoter slab, its `market_index` must be the
entry's market, and its `size_step` must be a nonzero multiple of the market's `order_step_size`. A
midpoint execute fills any step-aligned prefix of the ladder its quote published.
`AdminClient.getUpdateQuoterApprovedIx` takes trailing `responseAccount` and `stagedConfigHash`
arguments, and an approval requires both. `quoterConfigHash(quoterAccountData)`,
`QUOTER_CONFIG_SIZE` (736) and `AdminClient.getStagedQuoterConfigHash(quoter)` compute the hash.

`PerpMarketAccount.clobMarket` stores the book account, written once at registration, and
`PerpMarketAccount.quoterSlab` stores the slab. The program binds market, slab and book with
`has_one` on every accounts struct that names them, so a wrong account fails at the accounts layer.
`QuoterV0Account` also carries `bookTickSize`, `bookMinOrderSize` and
`bookDefaultActivationDelaySlots`, the book's placement rules, mirrored onto the entry by
`updatePerpMarketClobQuoter` so the fill and placement paths never CPI `order_rules_v0`. The attach
is the one remaining reader, and its `quoter` account is writable, so re-run it after changing a
book's rules. `QuoterSlabFull` (6405) and `QuoterNotOnSlab` (6406) are new errors. Attaching a CLOB
to a market requires a `min_cross_surplus` above zero.

`update_perp_market_step_size_and_tick_size` and `update_perp_market_min_order_size` hold the new
grid to the attached book's rules, as the attach does. On a market that designated a book they read
the quoter slab as the first remaining account, and the book and the CLOB program after it once the
book is attached. The SDK wrappers pass them through the new
`AdminClient.getAttachedBookGridAccounts(perpMarketIndex)`. The book's grid must equal the
market's, so neither of them can move the tick or step of a market with an attached book.
`update_perp_market_clob_book_config` takes the perp market writable and writes a new tick or step
to the market and the book together, so it is the only way to change that grid. The CLI form is
`clob-market update-config --tick-size` or `--step-size`.

`delete_initialized_perp_market` takes the market's quoter slab, and
`AdminClient.deleteInitializedPerpMarket` passes it. The program refuses to delete a market that
names a book, or whose slab holds an approved quoter, with `InvalidMarketAccountforDeletion`. The
slab and the book outlive the market, so a new market at the same index would adopt them.

## Attested flow and the activation delay

An activation-slot speed bump replaces JIT. On a book with a nonzero `default_activation_delay_slots`,
an order without the flow authority's attestation cannot fill against the book in the same
transaction. `placeAndTakePerpOrder` (v1) and unattested signed-message orders rest the whole order
taker-origin through the default window instead, and the cross cranks fill it. An immediate-or-cancel
order or a success condition on such a take is refused with `UnattestedSynchronousTake` (6404).
Cancels are never delayed, so a maker can always reprice ahead of unattested aggression. A book
with a zero default delay is unaffected.

Only a signed-message order can be attested. `placeSignedMsgTakerOrder` takes a `flowAttestation`
argument, which is swift's detached signature over the order's own signature plus an expiry,
verified in-program. The flow authority never signs a transaction, and an attested fill pays no
second signature fee. `placeAndTakePerpOrderV1`, `placeAndMakePerpOrderV1` and `modifyOrderV1` take
no flow-authority account. A maker or a modify that asks for an activation delay below the book's
default is refused with `UnattestedFastActivation` (6382). Swift's `POST /attest` takes
`{ "orderSignature": <base64, 64 bytes> }` and keys held orders by that signature, so swift and
keep-rs deploy together.

`VelocityCore`'s instruction statics are re-exports of the builders in `core/instructions/`, so each
one takes exactly what its builder takes. Every static keeps its name and call shape.

The quoter wire carries the verdict. `QuoteArgsV0` and `ExecuteArgsV0` carry `taker_served_window`,
which velocity sets for attested flow, and for the protocol cranks only when the orders they settle
rested at least two slots, so a zero-delay book cannot launder fresh flow into the flag by placing
and then cranking. The midpoint's `require_attested_flow` checks that flag instead of the
instructions sysvar, and its `quote_v0` and `execute_v0` account lists carry neither the sysvar nor
the velocity State account. `quoteRouter` takes `taker_served_window` as an argument, so a view for
unprotected flow shows no depth from a bumped book or a protected quoter, matching the fill's route.

## Order auctions are gone

An order no longer ramps its price from a start to an end over a duration. The ramp existed
because an order rested in the DLOB and competing fillers watched it cross their price. An order
routes to the book now, so the ramp only made the fill price depend on how long the sender took
to land the transaction.

`Order.price` is the worst price the order accepts, for every order type. A market order's `price`
is its cap, and the sender chooses how far from the oracle it sits. A market order that names no
price fills at most `oracle / unnamedPriceSlippageDivisor(contractTier)` from the oracle. A
signed-message market entry must name its price. An oracle-relative order holds the bound in
`oracle_price_offset`.

`OrderParams` and `ModifyOrderParams` lose `auctionDuration`, `auctionStartPrice` and
`auctionEndPrice`. `OrderParams` gains `activationDelaySlots`, which sets how long a rested
remainder waits before the book will take it. That is the duration knob the ramp used to serve. A
taker-origin remainder is crossed at the counterparty's price, so a longer wait can only improve
the fill. Take, make and signed-message orders all read it, and a value below the book's default
needs the flow-authority attestation. A trigger order refuses it. `maxTs` still only ends the
order. `placeAndMakePerpOrder`, `getPlaceAndMakePerpOrderIx` and
`buildPlaceAndMakePerpOrderInstruction` drop their separate `activationDelaySlots` argument, and
`modifyOrder` / `modifyOrderByUserOrderId` drop theirs. `getUserWithAuctionFilter` is removed,
because nothing sets `User.hasOpenAuction`.

On `Order`, `auctionStartPrice` and `auctionEndPrice` are renamed `clobNodeIndex` and
`clobOrderId`, which is what a placed-trigger shadow already stored in them, and `auctionDuration`
becomes `unusedAuctionDuration`. The account layout is unchanged.

`math/auction` is deleted. `math/worstPrice` replaces it with
`deriveWorstPrice(oraclePrice, contractTier, direction, namedPrice)`. The contract tier sets the
bound of an unnamed price: 2 percent on tier A, 5 on B and C, 10 on Speculative, and 20 on
HighlySpeculative and Isolated. A fired stop-market takes the same bound. A named worst price stays
absolute when the stop fires, so the fill cannot move it with the MM oracle.
`isFallbackAvailableLiquiditySource` moves to `math/orders`. `getLimitPrice`, `hasLimitPrice`,
`isRestingLimitOrder` and `isRestingSignedMsgLimitOrder` lose their auction and slot arguments,
`signedMsgOrderMaxSlot` trades its auction duration for `isRestingLimit`, and `hasAuctionPrice` is
removed. `SIGNED_MSG_FILL_WINDOW_MS` and `unnamedPriceSlippageDivisor` are new.

`placeAndTakePerpOrder`, `getPlaceAndTakePerpOrderIx` and
`preparePlaceAndTakePerpOrderWithAdditionalOrders` drop `auctionDurationPercentage`, so every later
positional argument moves one place left. `PlaceAndTakeOrderSuccessCondition` is a variant class
(`PARTIAL_FILL`, `FULL_FILL`), and `buildPlaceAndTakePerpOrderInstruction` takes `successCondition`
in place of the packed `optionalParams`.

Off-chain fill prediction must stop interpolating a price against elapsed slots and read the
order's own bound.

Nothing re-prices a signed order at placement any more, so the swift feed drops `will_sanitize`
from every order message and `SwiftOrderSubscriber.subscribe` drops its `acceptSanitized`
argument. A maker that filtered on the flag receives the flow it was filtering.

## Signed-message orders

A signed-message order is routed when it is placed, and whatever it cannot fill rests on the market's
book as a taker-origin remainder rather than in a slot. The activation window then decides
who fills it on price rather than on who lands a transaction first. A remainder that no book row
crosses is routed across the vAMM and the quoters by `crank_taker_origin_cross`, and two
remainders settle as a pair only while the vAMM can fill the earlier one, at a price it beats for
neither. On a book with a speed
bump, a keeper submits a signed message with its attestation; an unattested submission must be
signed by the taker's authority or delegate (`UnattestedSynchronousTake`), so a feed reader cannot
place a user's order ahead of the attestation. An entry whose order rests no longer blocks a delete
or shrink of `SignedMsgUserOrders` once it is past the eviction buffer.
`place_and_make_signed_msg_perp_order` is removed, because it existed only to match a signed-message
order already resting in `User.orders`.

The route digest is eight bytes on `SignedMsgUserOrders`, next to the CLOB order id the remainder
rests under. `getRouteDigest(route)` mirrors it, which a filler needs because the program rejects a
fill whose claimed route does not digest to what the order carries.

`SignedMsgOrderId` is 40 bytes, and `SignedMsgUserOrdersAccount` gains `version`. An account
created before the upgrade stores 24-byte entries. The program migrates it in place with fewer
entries, and `resizeSignedMsgUserOrders` restores its capacity. `decodeSignedMsgUserOrdersAccount`
reads both layouts, and the signed-message subscribers decode with it. A payer that is not the
authority can only grow the record.

The record is shared by every subaccount of an authority, so a uuid is spent per subaccount.
`SignedMsgOrderId.padding` is renamed `subAccountId`, and it names the subaccount that placed the
message. An entry migrated from the legacy layout carries `0xffff` and refuses its uuid to every
subaccount.

An entry's `maxSlot` is how long the record keeps its uuid, not the placement deadline. It is the
message slot for a resting limit, and the message slot plus `SIGNED_MSG_FILL_WINDOW_MAX_SLOTS` (150)
for any other order. That is at or after the deadline at every slot duration.
`SignedMsgOrderRecord.signedMsgOrderMaxSlot` still reports the deadline under the current slot
clock.

A taker signs `velocity-signed-msg:<program id base58>:` followed by the hex message. The
instruction and the swift request carry only the hex message, and the program adds the prefix
when it verifies. A signature over the bare hex message, or over another program's prefix, fails.
The prefix names the program, and its first byte is not a hex digit, so a Velocity signature does
not verify as a Drift swift message. `signSignedMsgOrderParamsMessage` signs the prefixed bytes.
`signedMsgDomainPrefix(programId)` and `signedMsgSigningBytes(programId, orderParams)` give those
bytes to a signer that does not use the client, such as a wallet `signMessage`. velocity-rs
exports `signed_msg_signing_bytes` and `SIGNED_MSG_DOMAIN_PREFIX` from `swift_order_subscriber`.

The record is the replay guard, and anyone can create it again empty. So
`deleteSignedMsgUserOrders`, and a `resizeSignedMsgUserOrders` that shrinks the record, fail with
`InvalidSignedMsgUserOrdersResize` while an entry is live. An entry is live while its order rests
on the book, and until it is more than 20 slots past its `maxSlot`. A shrink keeps the live
entries. The SDK creates a new record with `DEFAULT_SIGNED_MSG_USER_ORDERS_LEN` (32) entries, and
velocity-rs creates one with 32.

The signed payload must start with the Anchor discriminator of its message type,
`sha256("global:SignedMsgOrderParamsMessage")[..8]`, or the delegate message's when
`isDelegateSigner` is set. Any other payload fails with `InvalidMessageDataSize`. The program
exposes both as `PAYLOAD_DISCRIMINATOR`, and velocity-rs takes `SWIFT_MSG_PREFIX` and
`SWIFT_DELEGATE_MSG_PREFIX` from them. A message past its `maxTs` places nothing, and its
`maxMarginRatio` and `isolatedPositionDeposit` do not apply.

The entry must take or rest. A post-only entry fails with `InvalidOrderPostOnly`, and a trigger
entry with `InvalidSignedMsgOrderParam`. A `Market` entry must name its worst price, because an
unnamed price derives from the oracle at a landing slot the keeper chooses. One with a price of 0
fails with `InvalidOrderLimitPrice`. `signedMsgEntryOrderRefusal({ orderType, postOnly, price })`
mirrors the check, and `signSignedMsgOrderParamsMessage` throws on all three. swift refuses a
market entry with no price and an oracle entry with no offset. An entry that neither fills nor rests fails with
`SignedMsgEntryNeitherFilledNorRested` (6459), so the whole bundle reverts: its sidecars do not arm
and its uuid is not spent. `isDelegateSigner` for a user with no delegate fails with
`SigVerificationFailed`, as does a signature under a small-order key.

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
`VelocityCore.buildTriggerMarketOrderV1Instruction`, fire an armed trigger-market order and fill
it against the book in the same instruction, resting only the remainder as a taker-origin order.
Nothing lingers live in `User.orders`. `place_trigger_orders_v1` refuses a market with no CLOB
with `TriggerMarketHasNoClob` (6460). `trigger_market_order_v1` fires only a `TriggerMarket` on the
market it names. `trigger_limit_order_v1` takes `userStats` writable, because a cancel for
insufficient free collateral trips the equity breaker, and it records the trigger with its keeper
reward and trigger price before the place record.

`trigger_limit_order_v1` cancels a fired order that the book would refuse for its size, step,
price or expiry, and the keeper earns the flat reward. A refused placement would revert every crank
and hold the owner's later triggers behind it. A full book side is a state of the book, and an
eviction clears it, so both trigger endpoints fail on a full side with `MaxNumberOfOrders` and the
trigger stays armed. `trigger_market_order_v1` cancels a reduce-only stop-market with nothing to
reduce, with `ReduceOnlyOrderIncreasedPosition`, and pays the keeper nothing.

A placed stop-limit keeps its relay slot, parked on the non-trigger side and inactive. An eviction
re-arms the order and wakes that slot. The stop fires again only after the price crosses back
through its trigger. The crank that observes the recross earns the flat reward and points the watch
back at the trigger side. `trigger_limit_order_v1`, `crank_clob_evict` and
`crank_clob_remove_expired` therefore take the user's `["user_conditions", user]` PDA as a required
writable `triggerConditions` account, after `crankConditions`. A client that builds one of them
passes the PDA even when the account does not exist.

`resolveTriggerLimitOrderV1` and `resolveTriggerMarketOrderV1` take a `fired: FiredConditionArgV0`
argument and, after `perpMarket`, the read-only `state`, `quoterSlab`, `clobMarket` and
`clobProgram`. Each resolver stages only the order of the slot that fired, and reports no work for
every gate the executor applies that it can read: the fill pause, settlement, oracle validity,
TWAP divergence, a suspended book, a full side, a bankrupt account, and a reduce-only fire with
nothing to reduce. An account whose being-liquidated flag is set still stages a fire. The executor
clears a stale flag and fires, and refuses while the account is in liquidation. A refused fire no longer costs relay a failed simulation and its backoff.
`UserConditionsV0` grows to 7128 bytes to hold the nine resolver accounts per slot, so every user
is re-synced after the upgrade. `buildTriggerMarketOrderV1Instruction` takes `triggerConditions` as
required and an optional `solSpotMarket`; `trigger_limit_order_v1`, `crank_clob_evict` and
`crank_clob_remove_expired` take the same optional account, which program-keeper mode requires
when `State` names a SOL market, and a relay-paid removal or trigger crank charges
`max(flat fee, payment value)`. `placeTriggerOrders` refuses a batch that arms a ninth reduce-only
stop-loss. A signed-message bracket and a modify that arm a ninth armed reduce-only stop-loss fail
with `MaxNumberOfOrders` too, and the watch slots rank armed stop-losses first and parked stop-limits last. A fired
stop-market keeps its routed fill and cancels the remainder a full side refuses. A relay trigger crank pays at least the `min_payment` its slot
stores, so a lower market payment does not revert a slot synced before the change.

`sync_trigger_conditions` arms a watch on a market whose book is suspended, and the executors
still refuse to fire while the book takes no flow. The block holds eight slots, and the sync arms
reduce-only orders before the others, so a stop-loss is watched first. Every condition sync stores
its market accounts spot markets first and then perp markets, each sorted by index, whatever order
the caller passes.

The unfilled part of a take that does not rest emits `OrderActionRecord(Cancel)`, on the take,
signed-message and fired stop-market paths alike. Its explanation names the cause: an IOC remainder,
a spent reduce-only order, an account under liquidation, a size below the book minimum or the margin
gate.

A fired trigger rests taker-origin. It came to trade, so a cross settles at the counterparty's price
rather than picking it off at its own. Its owner cannot cancel it until `reservation_grace_slots`
after its activation slot.
Liquidation force-cancel stays exempt and `max_ts` still bounds its life.

An armed trigger past its own `max_ts` is dead, and both endpoints now treat it as no work
rather than firing it. The expiry sweep exempts anything that must be triggered, so such an order
sits in its slot until its owner cancels. Firing it moved nothing: the fill found the order
expired and the book refused to rest it, so the stop-market path paid the keeper for destroying an
order the owner could cancel for free, and the stop-limit path reverted. Relay's discovery skips
an expired trigger too, so a dead one no longer starves the armed triggers behind it on the same
account.

Both trigger endpoints refuse a market whose `PerpOperation::Fill` bit is paused. Firing commits the
order and pays the keeper out of the owner, so it takes the market gates any other step of the fill
lifecycle takes. `MarketStatus` carries no fill-paused variant, so a paused market still reads
`Active`. Both endpoints also refuse a market in settlement and any status other than `Active` or
`ReduceOnly`. On a `ReduceOnly` market, `trigger_market_order_v1` fires any stop-market and stamps
the fired order reduce-only, so its rest cannot add exposure. `trigger_limit_order_v1` fires only a
reduce-only trigger there.

Both trigger endpoints refuse, with `InvalidOracle`, an oracle that margin refuses: non-positive, too
volatile, too uncertain or stale for margin. A fire pays the keeper and cannot be undone, so it
judges the oracle by the set the fill it starts uses. Before, a stale or uncertain oracle could fire
a stop. `isOracleValidForTriggerOrder` mirrors the rule. The relay resolvers cannot read `State`, so
they can stage a fire on such an oracle. The executor's simulation then refuses it, and the turner
sends nothing.

`PerpPosition.reduceOnlyClobOrders` counts the reduce-only orders the owner has resting on the CLOB.
While it is nonzero the router caps that user's reduce-only fills to the position they reduce, so a
reduce-only stop can rest its remainder on the book without over-filling. A reduce-only order also
rests at most the position it reduces. A fired reduce-only trigger-limit rests only that much, and
one with no position left to reduce is cancelled with `ReduceOnlyOrderIncreasedPosition`.
`modify_order_v1` clamps a reduce-only replacement the same way, and refuses one with no position
left to reduce.

A resting remainder claims the depth it crosses, and claimed depth is withheld from every book read,
`quote_v0`, `quote_l3_v0`, `execute_v0` and `next_cross_v0`. A client therefore sees less depth than
the orders on the book suggest, and a take can fill less than a raw order listing implies. The claim
is what stops the remainder being frontrun: without it a taker buys the ask the remainder crosses and
reposts it worse, and the remainder pays the worse price.

The remainder itself is withheld whole from every ordinary read and fill for its whole life, before
and after its claim lapses, because the vAMM or a quoter can cross it where the book cannot see.
Only `crank_taker_origin_cross` fills it. `quote_l3_v0` reports such a row at size 0 with
`L3_ROW_FLAG_RESERVED`. Once the claim lapses its owner can cancel or modify it.

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
clears both takers' fees and the market's `min_cross_surplus` floor. A leg carries no price of its
own, so the crank bounds each leg at the last price inside the maker oracle band. It uses the
narrowest band among the quoters it consults, because the router drops a book whose quote reaches
past its entry's band. It refuses a leg that fills the owner of a takeable taker-origin order
(`CrossedTakerRemainderPending`), and a cross whose two legs fill one authority (`InvalidMaker`).

`crank_taker_origin_cross` requires the taker's `RevenueShareEscrow` when the taker carries a
builder referral, on the routed branch and on a settled pair. The referee discount and the referrer
reward bind on this path. When the escrow rides the crank, the builder fee binds too. The book keeps
the velocity order id as the order's `client_order_id`, so the crank finds the taker's builder row
by that id.

Every resolver refuses a call that marks an account writable beyond its staging region. A
resolver is a view: a turner simulates it, reads the staged call out of the simulated post-state,
and submits the real instruction separately, so landing one has to be inert. The staging region is
the shared relay scratch account, or `quote_buffer` for `quote_router`, plus the book's own
account for the two resolvers that ask the book for its resting orders, because `quote_l3_v0`
streams the answer into that account's response tail. A turner that marked anything else writable
now fails rather than being trusted not to matter.

`quote_router` no longer needs the perp market passed writable, and it no longer writes to the
makers it sizes. It was opening a position slot on a third party's account to reproduce the fill's
clamp and putting it back; it now sizes against the position a fill would open, so the makers ride
read-only. It applies the fill's oracle gate, band trim and CLOB-run drop, and cuts a Custom ladder
with the fill's own trim at the market step and the quoter room. It carries no caps, so it does not
apply the per-maker budget cap on CLOB depth, and it does not take a user's book claim off that
user's Custom quoter room.

`crank_cross_match` carries the SOL spot market, read-only, after the quote spot market, when
`State.solSpotMarketIndex` is not 0. It prices its keeper payment in quote at the live SOL oracle,
or else at that market's 5-minute TWAP. With no SOL market or no usable price it lands unpaid, at a
surplus floor of `min_cross_surplus`. The
`crank_taker_origin_cross` resolver stages the same market, because that crank prices its keeper
payment the same way.

A settled pair of taker-origin remainders applies the post-fill rules a routed fill applies: the
open-interest cap, the fill-price bands, the funding update, the 24-hour volume and the mark TWAP
sample. A reduce-only side shrinks the pair to its cover. The pair refuses a bankrupt party and a
counterparty under liquidation. A taker under liquidation proceeds only when a fresh margin check
takes it out of liquidation. The book serves claims oldest first, so `crank_taker_origin_cross`
settles a side's crosses oldest remainder first and fails with `NoTakerOriginCross` for a named taker
that is not the oldest aggressor on its side.

Relay's taker-origin crank fills a remainder against the makers it carries and stops short of any
other owner, so a book with more owners than the crank stages cannot block it. Two remainders settle
as a pair only while the vAMM can fill the earlier one. With `AmmFill` paused, the vAMM in drawdown
or the MM oracle divergent, the pair waits. A remainder that takes makers stops in front of a
remainder on the other side, and the two then settle as a pair at the earlier one's price. Two
lapsed remainders at the front of the book also settle as a pair. The keeper-payment shortfall
charged to a taker is capped by what the crank gained it against its rest price. A crank that
honours every claim may fill a signed-route remainder across the baseline. The crank refuses a
`cross_rows` read that ends on a row crossing the other side (`NoTakerOriginCross`).

A cross crank runs the market gates a routed fill runs. It refuses a market that is not `Active` or
`ReduceOnly`, in settlement, or fill-paused. When two crossed taker-origin remainders settle against
each other, velocity settles the pair itself rather than routing it, so it re-derives `reduce_only`
from the market the way a routed fill does. A row that rested while the market was `Active` carries
its own stale flag, and a market that has since flipped to `ReduceOnly` must not let that row grow a
position.

A reduce-only row whose owner holds nothing to reduce fills nothing, and the book still reports it
at full size. In a `ReduceOnly` market every row counts as reduce-only. `crank_taker_origin_cross`
cancels such a row when it is either side of the cross, so it does not block the newer remainders
on its side. The owner pays the flat removal fee. In program-keeper mode that fee is at least the
keeper payment's value in quote. The two-remainder path holds the counterparty price to the book
entry's maker band at the MM oracle price, which is the band the router uses.

## Relay cranks and their funding

Expiry, eviction, crossed books, crossed taker remainders, trigger arming and liquidations all land
with nobody submitting them. `ClobCrankConditionsV0` per market and `UserConditionsV0` per user hold
relay condition blocks and a keeper-payment reservoir, with simulation-only resolvers staging each
executor. `getUserConditionsPublicKey`, `getClobCrankConditionsPublicKey` and
`getRelayScratchPublicKey` are the PDA helpers. One resolver, `resolveClobCrank`, answers every one
of a market's CLOB crank conditions, because relay names the condition that fired. Its account
list carries the perp market, so the cross resolver can price the vAMM. `crank_cross_match`
settles funding once before its first leg and counts a period that rolls between the legs against
the surplus, and each leg refuses to consume a taker-origin row. `settle_pnl` on an expired market
settles over several calls when the user rests more than one sweep of book orders. The liquidation
payout is refused only to the liquidated user's authority. A transaction with no ComputeBudget price
is reimbursed the rails' priority ceiling once, shared across its liquidations, and a fill under the
$10 floor is reimbursed nothing.

A relay resync adds the markets a user entered after its sync, and the liquidation resolver reports
no work while the stored list lacks one. The stored list holds 48 entries, and eight book markets
with the quote market use 46. A relay liquidation or force cancel pays at least the payment its
liveness poll asserts. The resync resolver reports no work inside the paid interval. The liquidation
resolver skips the liquidation stage under `LiqPaused`, `FillPaused` or the market's liquidation
pause.

Every user gets a `UserConditionsV0`. `initializeUser` requires the `userConditions` account rather
than accepting `None`, and the payer funds its rent with the account. Relay can only watch an account
that exists, and the moment coverage matters is the moment somebody else's transaction gave the user
a position, such as a resting maker order filled by a keeper or a signed-message order submitted by a
filler, where the user is not a signer and no rent can be charged to them. Creating it up front is
also what makes every later sync permissionless, because the account is already paid for, so anyone
can keep it current. Vault velocity users are no exception, so `initializeVault` and
`initializeVaultWithProtocol` take `velocityUserConditions`.

`delete_user` and `force_delete_user` close the user's `UserConditionsV0` and send its lamports to
the user's authority. Both take the `["user_conditions", user]` PDA as a required writable account
after `revenue_share_escrow`. A user created before the account existed has none there, and the
close then does nothing. `getUserDeletionIx` and `getForceDeleteUserIx` pass it.

The protocol's own `User` and `UserStats` have the velocity signer PDA as their authority, which
cannot sign. `initialize_user` and `initialize_user_stats` for that authority therefore require the
payer to be the cold or warm admin, and fail with `Unauthorized` otherwise.

The condition syncs, `sync_liq_conditions`, `sync_user_conditions`, `sync_trigger_conditions` and
`resync_liq_conditions`, refuse an account passed twice and an account they cannot classify. Each
oracle must be the oracle of a passed market. Each exposed perp market and each market of a
stageable trigger order that has a CLOB must bring its crank conditions account and its quoter
slab. Each exposed perp market must bring its quote spot market, because the margin calculation
loads it. The default key does not count as a named oracle. Without the slab a staged liquidation
cannot sweep the user's book orders, and a sync that skips a trigger order also disarms its watch.

Relay fires only a trigger that `sync_trigger_conditions` armed. `buildSyncTriggerConditionsInstruction`
and `VelocityClient.getSyncTriggerConditionsIx` build the sync, and `placeTriggerOrders`, the bracket
orders of `preparePlaceAndTakePerpOrderWithAdditionalOrders`, and `modifyOrder` /
`modifyOrderByUserOrderId` on a trigger order append it.

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

A program-keeper crank draws reservoir lamports only when it collected a fee for the protocol, and
never when the payout account is the order owner's authority. A removal or trigger crank draws none
when no live SOL TWAP prices the payment, because the owner then pays only the flat fee. That covers the removal, trigger and
force-cancel cranks and `crank_taker_origin_cross`. A program-keeper `liquidate_perp_with_fill`
draws nothing when the payout account is the liquidated user's authority or delegate.
A full exchange halt waives the fee, so such a
crank draws nothing. `crank_taker_origin_cross` also requires the fees it collected to cover the
payment's value in quote, so two wallets that cross each other for no reward draw nothing.

Two cranks price themselves above that base, because a flat figure covers a quiet market and nothing
more. An expiry's offer climbs linearly with how long it went unclaimed, to 5,000 lamports over five
minutes. A liquidation repays the priority fee its keeper paid, for no more compute units than the
crank is measured to need, so a keeper is made whole without profiting either by inflating its limit
or by requesting less than it is repaid for. That repayment is capped at
`StateAccount.liquidationCrankReimbursementBps` of the recovery and converted through
`StateAccount.solSpotMarketIndex`, and it pays nothing extra when the program cannot price it. A
liquidation that neither fills nor sweeps book orders pays nothing. One that sweeps book orders but
fills less than the flat-payment floor pays the market's `force_cancel` figure. The relay resolver
stages a liquidation only when it can pay, and it prices the expected fill at an oracle the
executor accepts.
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
it. `updateCrankTreasury` and `fees set-crank-treasury --resync-floor <lamports>` also set
`resyncFloorLamports`, the balance above rent that paid resyncs leave for refills. Funding it is a
plain SOL transfer, and the sweep moves lamports back out of a retired or
over-provisioned market's reservoir, so they do not travel one way only. `clob-market init` takes no
`--fund-reservoir`.

A reservoir is held between two levels, both counted in cranks rather than lamports, so one setting
fits every market. `refillTargetCranks` is how full a refill leaves it and `refillWatermarkCranks` is
when a refill wakes. The watermark must cover the refill's own round trip, because a turner polls,
simulates and lands it while the reservoir keeps paying, and a market-wide move is when cranks fire
fastest and the network is slowest to land one. The target is read at refill time and reaches every
market at once, and a refill always takes a reservoir to at least its stored watermark plus one
maximum crank payment. The watermark is resolved to lamports at attach and stored on the market
(`ClobCrankConditionsV0.refillWatermarkLamports`), because it is the threshold that market's wake
condition carries, so it reaches a market on its next attach. What a refill pays is priced from the
rails like every other crank and stored on the market it fills (`CrankPaymentsV0.refill`,
`--crank-cu-refill`), because a condition has to advertise a floor a turner can filter on.

Opting a user into self-maintaining liquidation conditions states what its resync pays, and the
treasury is the payer, so that figure is capped (`LIQ_SYNC_MAX_COST_UNITS`) and drawn at most once
per `syncFallbackSlots` (`UserConditionsV0.lastPaidSyncSlot`). Only the user's authority, its
delegate or the warm or cold admin sets or clears paid terms. A third party can sync only a block
that holds no paid terms, and only to write none, or it fails with
`SelfSyncTermsNeedUserAuthority` (6462). A resync pays only when the user's positions digest
changed. Resyncs in one transaction divide one payment, so `resync_liq_conditions` takes a trailing
`instructions_sysvar` account to count them. The sysvar lists only top-level instructions, so a
resync invoked through CPI pays nothing.
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
A market with more quoters than one transaction can carry is read in passes. `quoteRouter`
consults the slots a fill would consult, and like a fill it refuses a tail that carries more than
`MAX_ROUTE_QUOTERS` (8) of them with `TooManyQuotersConsulted`.

`TopMakersClient` reads the dlob-server's `/topMakers` and returns the `MakerInfo[]` a routed
placement must carry for the book's best resting owners on one side. It returns an empty list when
the request fails and skips a maker whose account does not load, so a caller loses that maker's
depth rather than the fill.

## Liquidation and the mark TWAP

A liquidation cancels the account's book orders in its scope itself, so `forceCancelClobOrders` is
not a step before it. Every liquidation builder, `getSetUserStatusToBeingLiquidatedIx` and
`getForceDeleteUserIx` append the books through `getLiquidationBookMetas(userAccount)`.
`clobResidentOpenOrders` mirrors the count the program reads. Without a market's book the
liquidation latches the account, cancels what it can and succeeds without a transfer, and a later
call continues. A liquidation with a swap fails with `LiquidationConflictsWithClobOrders` instead.

A settle of an expired position takes the user's book orders in that market off the book first.
`settle_pnl` and `settle_multiple_pnls` read, after the revenue-share accounts, each market's quoter
slab (read-only) and book (writable), then the CLOB program once. `settlePNLIx` and
`settleMultiplePNLsIx` attach them for each market in settlement where the user rests book orders.
Without them the settle fails with `PerpMarketSettlementUserHasOpenOrders`.

`liquidatePerpWithFill` fills its forced order through the router, so a liquidation reaches the
market's CLOB and its PropAMM quoters rather than only the makers the caller passes.
`getLiquidatePerpWithFillIx` appends the market's quoter section to the remaining accounts and takes
an `extraQuoterAccounts` argument for further quoters. A market that names a book refuses the call
without that section, and book depth is reachable only for owners the transaction carries, so pass
the book's resting owners in `makerInfos`.

A fill samples the mark TWAP at its average price over every source, so a fill that trades only on
a book records the book's price.

A fill's funding update judges the oracle TWAPs as they stood when the instruction began, the same
values its price-band checks read. The fill advances the TWAPs toward the live oracle price before
it updates funding. Before, a fill at a funding boundary could pull a lagging 5-minute TWAP inside
the mark divergence limit and write funding that the funding crank would refuse. `blockOperation`
reads the TWAPs from the market account, which is the state the instruction starts from, so it
predicts the gate unchanged.

`updatePerpBidAskTwap` takes `quoterSlab`, `clobMarket` and `clobProgram`, and estimates each side of
the market from the book alone. It names no counterparties, and both it and
`getUpdatePerpBidAskTwapIx` lost their `makers` argument: the program reads the book over CPI and
never looked at a passed `User`. `getUpdatePerpBidAskTwapIx` resolves the three book accounts from
the market, so a caller supplies only the market index. A market that names a book refuses the crank
without that section, a suspended book moves no mark, and a quote that has not rested for
`BID_ASK_TWAP_MIN_QUOTE_REST` is dropped.

## Other surface changes

Velocity's own events are versioned. `ProtocolUserWithdrawRecordV0` and
`AcceleratedReferralStatusChangedRecordV0` carry their own discriminators, and the new accounts keep
reserved tail space.

New endpoints take a single args struct (`PlaceAndTakePerpOrderV1Args`, `TriggerMarketOrderV1Args`,
`PlaceTriggerOrdersV1Args`, `UpdateQuoterApprovedArgs` and the rest).

`AdminClient` gains the quoter registry builders the CLI needs: `getInitializeQuoterIx`,
`getInitializeQuoterSlabIx`, `getUpdateQuoterAccountsIx`, `getUpdateQuoterApprovedIx`,
`getUpdatePerpMarketClobQuoterIx`, `getUpdatePerpMarketClobBookConfigIx` and
`getResizePerpMarketClobBookIx`. The market's quoter slab is the config authority of every book
velocity attaches, so the last two are the only way to change a book's rules or grow its arena.
`getInitializeProtocolUserIxs(name, payer)` creates the protocol `User` and its `UserStats` under
the velocity signer PDA, and `payer` must be the cold or warm admin. New PDA helpers:
`getQuoterCrossConditionsPublicKey` and `getProgramDataAddress`, beside the exported
`BPF_LOADER_UPGRADEABLE_ID`.

`HotRole.ConditionsSync` is a new hot role, stored in `StateAccount.hotConditionsSync`. Its key may
set paid resync terms when it syncs another user's conditions, as the warm admin may.
`auth set-hot-admin conditionsSync <key>` sets it. The pause admin may clear any hot role, by writing
the default key with `updateHotAdmin` or `auth set-hot-admin`, and cannot set one.

`exchange set-status --multisig` records the live status in the proposal, and `multisig execute`
refuses a status write that would clear a pause added after it was proposed.

The admin CLI gains the `quoter` and `clob-market` command groups plus `fees withdraw-protocol-user`
and `user init-protocol`, which creates the protocol `User` and its `UserStats` with a warm or cold
payer. `update_perp_market_clob_quoter` refuses an attach whose derived crank payments are all zero,
so the fee rails are set before a book is attached. The attach takes the market's quote spot market
and stores its oracle on the crank conditions, so a staged cross carries it. Without it, a cross on a
market whose quote is priced by an oracle account fails with `OracleNotFound`.
`clob-market update-config` retunes a live book's mutable config through velocity, and
`clob-market resize` grows its arena, to at most 1024 slots. `clob-market init` defaults
`--capacity` to 1024, `--evict-threshold` to a quarter of `--capacity`, and `--blocking-min-size`
to 1000000. It refuses an `--evict-threshold` that is not above zero and below half of
`--capacity`, which the book refuses, before it sends anything. The
blocking floor must be at least ten times the larger of the book's and the market's minimum order
size.
The CLI creates a market's slab before it registers that market's book, and passes the registry's
accounts on register, approve, set-active, set-config, set-accounts and set-watch.
`quoter set-approved` passes the entry's response account and the hash of the entry it reads, and
`--config-hash <hex>` refuses to send when that hash differs from the reviewed one. `clob-market init`
approves in a second transaction, after the entry exists. `quoter attach-cross --fallback-slots` takes a
value other than 1500 only from the maker's authority.

## Vaults

The vaults program adds `mark_user_vault_owned`, which flags the velocity `User` of a vault created
before initialization set `UserStatus::VaultOwned`, so the revenue-share sweep no longer credits it.
The vault manager or the vaults admin signs. The flag is set only, so a repeated call does nothing.
`VaultClient.markUserVaultOwned(vault)` and `getMarkUserVaultOwnedIx(vault)` wrap it.
