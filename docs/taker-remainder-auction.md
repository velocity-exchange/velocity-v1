# Taker remainders on the CLOB, and the activation-slot auction

An unfilled taker remainder rests on the market's CLOB behind an activation delay, and the delay
is an auction. This document states the rules that make resting there safe, numbered R1 to R8, and
the maker-priority gate that shares the same activation delay. The surrounding design lives in the
Notion PropAMM doc.

## The problem this solves

A signed-message taker order cannot be IOC, because `place_signed_msg_taker_order` rejects that
shape. Anything it did not fill therefore used to rest **in `user.orders`, on the DLOB**, and
nothing migrated it. The only paths that moved a remainder onto the CLOB were `place_and_take_v1`
and `place_and_make_v1`, because they were the only ones holding CLOB accounts. A keeper-driven
fill (`fill_perp_order`) had neither the accounts nor a migration step.

That left the rule "if it can rest and be matched, it lives on the CLOB" with an exception nobody
intended, on the highest-volume case: every swift order that does not fully fill on placement.

## Why ordinary resting is not enough

The obvious answer is to migrate the remainder, let it rest at its own price, and let
counterparties take it. That is how a book normally works, and for an order whose price the user
chose it is exactly right.

It fails for an order resting at a *slippage bound*. A swift market order's only price is
`auction_end_price`, the worst fill it agreed to tolerate. Resting there writes a free option. It
gets hit precisely when hitting it is profitable for someone else.

The competition to hit it is also the wrong kind. An order becomes matchable at its
`activation_slot`, so capturing it by *taking* means landing a transaction in that slot. The
winner is whoever pays the highest priority fee and has the best landing infrastructure, and they
take it at the taker's limit. **Taking is a landing race, and the improvement accrues to
infrastructure.**

Resting is a price race. A counterparty that wants the order must be on the book *before* the
activation slot, where orders are ordered by price and the outcome is deterministic. Being fast
buys nothing, and quoting the best price wins. That is what the activation slot is for, and it is
the mechanism these rules lean on.

## Mechanism

The remainder rests on the CLOB with an activation delay. That delay is the auction window:
makers line up inside it, and at the activation slot the best-priced resting counterparty gets
the fill, at **its own price**, so the taker captures the whole improvement.

Concretely, taker's bid rests at 101; makers rest asks at 99 and 100 during the window. At
activation the 99 ask matches: the taker pays 99, the maker gets the 99 it asked for, and the
maker that quoted 100 loses on price rather than on latency.

### Rules

**R1. Migration.** A fill migrates a restable remainder to the market's CLOB instead of leaving
it on the DLOB. Restable means what it already means on the `place_and_take_v1` path: a fixed
price, no oracle offset, not reduce-only. The rest price and the activation delay are R6 and R7.

Every order that comes to trade and does not fill ends up here. A signed-message order routes at
placement and rests its remainder on the book, so the highest-volume order type in the protocol no
longer rests on the DLOB. A fired trigger rests here too: it is an order that came to trade, so it
gets the counterparty's price in a cross rather than being picked off at its own, and the
taker-origin crank is what carries a route to it.

**R2. Taker-origin marker.** A migrated remainder is flagged on the book. `OrderNodeV0.bit_flags`
already exists (`Open`, `Ask`), so this costs no space. The flag means: *this order demands
liquidity; in a cross it is the aggressor.*

**R3. Cross pricing, in velocity.** When a taker-origin order crosses a resting counterparty, the
match settles at the **counterparty's** price. Best price on the book wins, and price priority
already orders the book that way, so there is no new selection logic. Only the price the match
settles at changes.

**When both sides are taker-origin, price-time priority decides which is the counterparty**: the
order that rested first is the maker and its price is the settlement price; the later arrival is the
aggressor and crosses into it. Rest order is the CLOB order id alone. A book's `next_order_id`
only increases and never reuses a value, so the lower id rested first and no separate slot
comparison is needed.

Nothing else about R3 changes. The later order is the one that came to trade, which is what
"taker" means everywhere else in this document, and the earlier order gets the price it was
already offering, which is all a maker is ever promised. The alternative was the midpoint, and it
is worse in the way that matters. It takes half the improvement away from the order that earned it
and hands it to one that was already content, and it makes the outcome depend on a number neither
party quoted.

One consequence is worth stating. The *earlier* order captures nothing here: no improvement, and
no rebate beyond the ordinary maker rebate. It gets the price it was already offering.

**A partly filled remainder keeps its id and its queue position.** The book API for editing a
resting order's size is `fill_v0`. Velocity reports the base it settled and the order shrinks
in place. The alternative is to cancel the order and place a new one for the leftover. That
settles the same base at the same price and still charges the taker for it, because the
replacement takes a fresh order id and goes to the back of its price level, behind every order
that arrived while the first one was resting. A taker that waited for its turn would lose it by
being filled.

**The CLOB does not do this.** Every order fills at its own stored price, and the book neither
reprices a cross nor resolves one. Velocity already computes fill prices at settlement, so all the
CLOB owes it is the taker-origin *fact*, reported on `RemovedOrderV0`. R4 explains why that is the
wire that carries it.

Settling at the counterparty's price is not what `cross_match` does. `cross_match` fills each leg
at its own price and hands the difference to the protocol `User`, which is the surplus
`min_cross_surplus` floors. For a taker-origin leg there is no surplus to hand anywhere, because
it belongs to the taker. A taker-origin cross is also simpler than that crank: an ordinary
two-user match at one price, needing none of `cross_match`'s ephemeral protocol-taker machinery.

**R4. A crossed taker remainder cannot be taken, and neither can the depth it crosses.** This is
load-bearing, not an optimisation. Without the first half, an outsider who lands a transaction at
the activation slot takes the taker-origin order at its limit and pockets the improvement. Without
the second half, the outsider takes the counterparty instead and rests a worse one in its place.
Either way the landing race is back, with an extra step.

**Holding the remainder back is not enough. The remainder claims the depth it crosses.**
Withholding only the remainder would leave the counterparty an ordinary maker anyone could take,
which reopens the landing race one step further out. With asks at 100 and 101 and a taker-origin
bid at 102, an outsider takes the 100 ask and rests a new ask at 101. The remainder then crosses
101, and the outsider keeps the difference. The remainder must therefore claim its counterparty
rather than merely be withheld from it.

A taker-origin order claims the base it crosses, best price first. Claimed base is withheld from
`quote_v0`, `quote_l3_v0`, `execute_v0` and `next_cross_v0`, which is the whole matchable set,
for every caller except the crank that owes the taker its improvement. That crank asks for it with
`consume_reservation`. A taker sweeping the side sees the unclaimed remainder of each order and
fills that.

Reservations are ordered, and the order is exact. Every taker-origin order on a side is threaded
onto its own list in rest order, and reservations are allocated down that list, so the remainder
that rested first takes the best depth. The book stores no reservation state. It is computed in
the same walk the quote or the fill already does, from the list and the two prices. A reservation
therefore cannot disagree with the book, a cancelled counterparty stops being claimed, and depth
nobody crosses is never held.

Narrowed to exactly the harm, in three ways:

- Only a **taker-origin** order claims. An ordinary maker×maker cross is unclaimed arbitrage, not
  somebody's improvement, and a reservation over it would cost takers depth for nothing.
- A reservation reaches only depth the claimant's own price crosses. A remainder that crosses
  nothing claims nothing and is ordinary depth, quotable and takeable at its own price. That is
  the fallback when no maker lines up during the window, and it is how the remainder eventually
  fills if none ever does.
- A reservation lapses `reservation_grace_slots` after the claimant activates, 32 on a fresh
  market and settable by `update_market_v0` up to 150. Past that the depth is ordinary again. A
  reservation is what makes the auction deterministic, and a crank has to run to collect it, so a
  crank that never runs must not hold the top of the book forever.

An order still inside its activation delay, or already expired, is not a counterparty. Nothing
can match it, so no improvement is within reach, and claiming it would cost the book that depth
for its whole auction window, which is exactly when it is resting there.

The counterparty is never skipped, only claimed. Consuming it is the fill velocity's cross
resolution runs. It takes the counterparty's side with `execute_v0` under `consume_reservation`,
which is an ordinary fill at its own price, settles the pair internally at the counterparty's
price, and reports the settled base back with `fill_v0` so the remainder shrinks in place.

**A reservation outranks price, including a better-priced order on the claimant's own side.** A
remainder bidding 101 claims the 99 ask it crosses, and a maker that then bids 102 takes nothing
from that ask while the claim stands, though its price is better. The claim is not a quote
competing on price; it is depth already spoken for by an order that came to trade and rested
first.

That priority has a real cost. With a maker bid in front of it, the top of the book is maker
against maker, so the taker-origin crank refuses it, because neither head is taker-origin, while
the arb crank cannot reach the claimed ask. Both cranks go quiet and the book rests crossed. This
is bounded rather than a deadlock. The claim lapses `reservation_grace_slots` after the remainder
activates, and the arb crank clears the front then. A market that finds the wait too long lowers
the grace window through `update_market_v0`.

**Attested flow reaches unclaimed depth only.** Attestation buys a synchronous fill against the
book. It does not buy claimed depth. An attested taker lifting the cover a remainder crosses is
exactly the frontrun the claim exists to stop, because it takes the makes at the maker's price and
leaves the remainder to cross worse, so the claim holds against it like any other taker. The
consequence for a market operator is that a resting remainder can shut attested takers out of the
depth it claims until a crank resolves the cross or the claim lapses, and an attested take that
reaches nothing rests as a remainder of its own.

**A remainder cannot be cancelled inside its window.** Binding the taker is what makes the auction
an auction. An order its owner can pull the moment a maker lines up offers nothing to line up
against. `Book::cancel` refuses while `slot < activation_slot` for a taker-origin node, and the
rule lives in the CLOB because that is where the flag and the slot are. Liquidation force-cancel
passes `force` and stays exempt, so a distressed account is never blocked, and `max_ts` still
bounds the order's life. The cost is real. A taker who signed a market order cannot pull it for
the window, and neither can a delegate.

**Every reader withholds through one function, and that is the point.** A router allocates from
the quote and velocity binds the execute to it. Depth one of them offers and the other withholds
is a reverted transaction for a taker that did nothing wrong, because velocity cannot route around
a shortfall after the fact. The ladder, the L3 rows, the fill, and the cross report all compute
the reservation with the same walk over the same list, so they cannot disagree. `quote_l3_v0`
withholds claimed base like the rest and flags the row it trimmed.

### Why withhold rather than fail the call

Three shapes were possible. **Uncross inside `execute_v0`** is unavailable. It would return fills
outside the prefix the router quoted, which velocity's quote-to-execute binding refuses. **Fail
the call** protects the remainder equally well, but it makes the order shadow every level behind
it on its side. Quote could only publish the prefix in front of it, because anything further was
not deliverable: the fill would hit the remainder and revert. A taker remainder rests at a
slippage bound, so it normally sits at or near the front, and one crossed remainder took its whole
side dark until the crank resolved the cross.

**Withholding the claimed base** keeps the protection and drops the cliff. Neither the remainder
nor the depth it claims can be taken. Everything else on both sides stays quotable and fillable,
because execute can deliver it. Consistency between the two is structural rather than maintained,
because every reader computes the reservation the same way. A taker finds less depth than it hoped
for, which is ordinary book behaviour. Depth vanishes between quote and fill all the time.

Withholding is also what a partial reservation needs. A remainder smaller than its counterparty
claims only part of it, and the rest of that order stays takeable. Failing the call, or skipping
the whole order, would have had to round one way or the other.

Nothing emits `ClobError::TakerOriginCrossPending` any more. It is deprecated in place rather than
removed, because the numeric code is the on-chain identity of every variant after it.

**R5. The cranker is paid out of the improvement.** The reward is an ordinary filler reward,
sized by `calculate_filler_reward`, so roughly a tenth of the taker fee. It is charged to the
taker in quote rather than carved out of the taker fee. The improvement belongs to the taker, and
widening the taker fee on this path would couple the crank's cost to the protocol fee schedule,
which changes for unrelated reasons.

The invariant is that `maker_price + taker_fee + crank_fee` must be better for the taker than
`rest_price + taker_fee`. The reward is capped by that, and it is paid **in full or not at all**.
An improvement that cannot cover it resolves the cross for free rather than paying a shaved
reward. That keeps the cranker's revenue predictable and stops the taker funding a keeper subsidy
out of a gain that could not have funded one.

An equal-price cross, and a dust-improvement cross, which is the same case, therefore resolves
rather than resting. Firing only when the improvement exceeds the fee costs more than it saves.
The remainder is gated against being taken, so refusing to cross leaves it with nothing able to
clear the gate, and a single unit of dust improvement in front of it would strand it for its whole
life. The taker asked to trade, and a fill at a price it already accepted is what it wanted. The
cranker is not working for nothing either way. The market's reservoir pays its lamports, exactly
as it does for every other CLOB crank.

The one hard refusal is a cross that would leave the taker *worse off* than resting. Reaching that
takes a taker fee above 100%, because the extra fee on the better price is `rate x improvement`.

**R6. Rest price.** For a limit remainder, the rest price is the order's own limit. For a market
remainder it is `auction_end_price`: the order's own `price` is zero, and the auction end is the
worst fill it already agreed to. R2 to R5 are what make resting there safe rather than a free
option. The order cannot be taken while a counterparty crosses it, and a cross settles at the
counterparty's price, so a maker arriving in the activation window competes on price rather than
on transaction landing. Oracle-offset orders do not migrate, because an oracle-floating price has
nothing fixed to rest at.

**The route rides the order's own record, not the book.** A signed-message remainder's quoter list
is stored on `SignedMsgUserOrders` alongside the CLOB order id it rests under, because the route
is velocity's concept and the book has no registry to check it against. The CLOB knows tick, step,
capacity, and activation. `taker_origin` earns its place on `OrderNodeV0` because it changes what
the book does, and a route digest changes nothing there, so the digest is not a node field. Off
the `Order` struct the digest is no longer squeezed into five spare bytes either. It is eight, the
width the collision analysis wanted.

A market remainder only exists when the taker's bound is tighter than the vAMM's price. The vAMM
is in every fill's mandatory baseline and quotes deep enough to absorb a market order outright, so
a taker with room to reach the curve fills. That is also the case where resting at the bound
matters most, because it is the one the taker cannot otherwise complete.

**R7. Activation delay.** The market's `default_activation_delay_slots`, as with any placement.

A market at 0 is a supported setting rather than a broken one. R4's reservation protects the
improvement, not the delay, and it keys on whether a live counterparty crosses the order rather
than on the clock. At 0 a remainder claims its counterparty the moment it rests, so a remainder is
takeable at its own price only while nothing crosses it, which is exactly when there is no
improvement to take.

The delay does set the clock a reservation expires on. It lapses `reservation_grace_slots` after
the claimant activates, so at 0 the window is the grace alone.

A nonzero delay buys one thing: a pre-window in which the order is not matchable at all, so makers
can line up before anyone trades with it. It costs one thing: the owner cannot cancel until the
window ends, because the cancel refusal keys on the same activation slot. A market that wants
makers to compete before the first fill pays that cost. A market that wants immediacy sets 0, and
the taker keeps the right to pull its order at any time.

**R8. Discovery, on the conditions that already exist.** The crank is permissionless, so something
has to notice a resolvable cross; relay turners do, through the market's `CLOB_CRANK_CROSS`
conditions, whose resolver stages `crank_taker_origin_cross` when the top of the matchable book is a
crossed remainder and `crank_cross_match` otherwise. A `ResolvedCrankV0` names its own executor, so
one condition slot serves both.

No new slot and no new watch. A taker-origin cross can newly appear for one of two reasons. A
side's best moved, which the cross condition's 8-byte `OnAccountChange` over `best_bid` and
`best_ask` catches, because a crossing order is by definition a new best. Or a front-of-book order
reached its `activation_slot`, which the cross-activation `AtSlot` hint names precisely. That hint
is min-folded at every placement, including a migrating remainder's.

The ordering is the economics rather than a preference. The improvement belongs to the order that
came to trade, so it is handed over before the protocol middles the same crossed book as
arbitrage. It also keeps the arb crank off a cross it must not run, as
[What this replaces](#what-this-replaces) describes.

`min_payment` on those conditions stays the market's `keeper_payment_lamports`. That is what relay
measures, since `assert_paid_v0` watches the payout account's lamport balance, and this crank pays
the same reservoir lamports as every other. Pricing it above that would make exactly the crosses
R5 resolves for free undiscoverable, and a unit of dust in front of a gated remainder is enough to
strand it for its whole life.

The wake that needs care is the **both-remainders** case. Two remainders can only face each other
while something crosses the earlier one, so the moment the pair becomes resolvable is usually the
*blocker's* removal rather than a new order's arrival. That moves a head pointer and fires the
change-watch whenever the blocker is its side's head, which is the ordinary case. It misses two
shapes. A blocker sitting behind a better-priced order nothing can match yet rewrites an arena link
on removal rather than the head. A blocker that leaves the matchable set by passing its own
`max_ts` is no account write at all. The expire condition's `AtTimestamp` hint covers the second
one hop earlier, because it fires at that `max_ts` and removing the expired order then moves the
head. The every-slots cross fallback is the floor under both, so a missed hint costs latency
rather than liveness.

## Maker priority: the take-side gate

The rules above protect the *taker's* remainder. The same activation delay also protects makers,
through a gate on the other side of the trade: on a book with a nonzero
`default_activation_delay_slots`, only attested flow may fill against the book in the same
transaction. Attestation has two transports, both from `State.hot_flow_authority`: the key signs
a swift-built transaction as a named `flow_authority` account, or it signs a detached
`FlowAttestationV0` over a signed-message order's own signature, verified in-program, so a
keeper-built fill carries proof without the key ever signing a transaction it did not build, and
without a second signature fee. An unattested marketable order does not take. It rests whole,
taker-origin, through the default window, and the cross cranks fill it. A maker crosses it at the
maker's own price, or it activates and becomes ordinary depth.

Cancels are never delayed. That asymmetry is the property: a maker can always reprice ahead of
aggression it never agreed to fill instantly, so informed flow cannot pick off a stale quote in a
landing race. Attested retail flow, the flow makers want, keeps its synchronous fill.

Enforced velocity-side, in layers. The taker routes (`place_signed_msg_taker_order`,
`place_and_take_perp_order_v1`, `trigger_market_order_v1`'s staged tail) skip their fill leg
entirely for an unattested transaction and rest the whole order. A
`place_and_take` shape that demands a synchronous outcome an unattested transaction cannot have,
such as an IOC or a success condition, is refused with `UnattestedSynchronousTake` rather than
rested without an error. The protocol cranks (`crank_cross_match`, `crank_taker_origin_cross`) do not vouch for
flow by construction: a crank measures it. `crank_taker_origin_cross` reads the rest time of the
remainder it settles, and `crank_cross_match` reads both sides it sweeps and takes the worse
answer, bounded by the size the cross takes. One verdict covers both of its legs, because a cross
is one event on two sides: a leg that judged only the side it sweeps would call the flow
protected whenever the fresh order sat on the other side, and the cross would then reach
liquidity that serves protected flow only. Liquidation fills vouch by measurement too: they check
that the depth the fill can reach has rested, the way the cross cranks do.

Velocity verifies the attestation once, at its own boundary, and forwards the verdict to every
quoter on the wire: `QuoteArgsV0.taker_served_window` says the taker's flow served a protection
window, either the swift hold or measured rest on the book. The cranks do not vouch by
construction.
on a zero-delay book, "rested through placement" is a zero-length window, and place-then-crank is
two back-to-back transactions. A crank marks the flow protected only when the orders it settles
rested at least `SERVED_WINDOW_MIN_SLOTS` (two slots, above the swift hold), so a zero-delay book
cannot launder fresh flow into the flag. A quoter that only serves protected flow (the midpoint's
`require_attested_flow`) checks that flag and nothing else; it trusts velocity for it the way it
trusts `users` and `caps`, because velocity signs the CPI and settles the fills. This is also what
lets a rested unattested order reach protected liquidity: the crank that fills it carries the
proof the taker earned by waiting. The off-chain router view (`quote_router`) takes the same
fact as an argument, so a client prices exactly the books its flow will reach.

A book with a zero default delay opts out of all of it: every taker is synchronous there, as
before.

## What this replaces

`cross_match`'s protocol-as-middleman only makes sense for two *maker* orders crossing, where
neither side is demanding liquidity and the spread is genuinely unclaimed arbitrage. Once
taker-origin crosses price at the maker's side, the protocol's cut disappears from the case that
matters most for user outcomes and remains only for maker×maker crosses. `min_cross_surplus`
keeps floor-guarding those.

**A crossed remainder is not depth the arb crank may cross, and its discovery no longer offers it
one.** The cross resolver's crossing-prefix walk steps over a taker-origin node instead of counting
its base. Both outcomes of counting it are wrong: usually the book withholds the remainder from
`execute_v0`, the leg steps over it, the two legs imbalance, and the ordinary cross *in front of*
the remainder is stuck for as long as it rests there; and when the first leg consumes the whole
opposite side, nothing crosses the remainder any more by the time the second leg runs, so the book
hands it over at its own resting price with the improvement going to the protocol `User`. That is
the landing-race outcome R4 exists to prevent, arrived at from the other direction. Stepping over it
restores the composition: the arb crank clears the front of the book, and the remainder's own cross
is what the resolver answers with next.

**A hand-built `crank_cross_match` needs no rule of its own.** The instruction is permissionless,
so the resolver's walk stepping over a remainder is not enough on its own. The caller controls the
account list, not the walk. A velocity-side predicate used to carry that load, and it could
only approximate the harm: it refused a cross that consumed a remainder's *entire* crossing depth,
while a smaller cross still ate the best of that depth and left the remainder crossing the worse
rest, since both walks go best price first. The claim removes the need for the predicate. Claimed
base is not in the arb crank's matchable set at all, so the cross cannot reach it whatever size
the caller asks for, and the remainder keeps the whole improvement rather than the part nobody
took first.

## What each program carries

**CLOB (`anchor-v2/programs/clob`)**
- `OrderBitFlag::TakerOrigin` (bit 4) plus `PlaceOrderArgsV0::taker_origin` to set it.
- `RemovedOrderV0::taker_origin`, so `cancel_order_v0`/`evict_worst_v0`/`remove_expired_v0`
  report the flag. That is the only place the CLOB reports it, and it is enough: R4 keeps a
  taker-origin cross out of `execute_v0` entirely, so both sides of a cross velocity settles
  leave the book through a removal. The shared quoter-interface types (`UserBalanceChangeV0`,
  `CancelledRemainderV0`) are untouched, because every quoter emits those and only the CLOB can
  ever have an order to mark.
- R4's claim. `OrderNodeV0` carries `taker_origin_prev` / `taker_origin_next` and the header
  carries a head, tail and count per side, so every taker-origin order on a side is one list in
  rest order. One function walks that list against the cover side and reports the claimed base
  per order; `quote_v0`, `quote_l3_v0`, `execute_v0` and `next_cross_v0` all withhold it, so
  quote never publishes depth the fill will not deliver, the rest of the side stays tradeable,
  and the four readers cannot disagree. `ClobHeaderV0::reservation_grace_slots` is when a
  reservation lapses. There is no separate `TakerOriginGate`. Asking whether a remainder is
  crossed and asking whether cover is claimed are the same computation, so one function answers
  both rather than a gate sitting beside a reservation.
- `QuoteArgsV0::consume_reservation` / `ExecuteArgsV0::consume_reservation`, which
  `crank_taker_origin_cross` sets to reach claimed depth. The book trusts the flag the way it
  trusts `users`, `caps` and `taker_served_window`: velocity signs the CPI and settles the fills.
- No stored reservation. The claim is a function of the list and the two prices, recomputed by
  each reader, so a cancelled counterparty stops being claimed with no bookkeeping, and depth no
  claimant crosses is never withheld.
- `ClobError::TakerOriginCrossPending`, deprecated in place. Nothing emits it, and the numeric
  code cannot be reused.
- `fill_v0`: velocity reports base it settled against a resting order and the order shrinks in
  place, keeping its id and its queue position. This is what lets a cross resolve without
  cancel-and-replace. It emits the existing `ExecuteRecordV0`, because it is a fill, so a consumer
  that already decodes fills needs nothing new.
- `force` on `CancelOrderArgsV0` / `CancelAllArgsV0`, and a `slot` for the window check, so
  liquidation force-cancel can reach a bound remainder and an ordinary cancel cannot.
- `L3RowV0` carries `node_index` and `placed_slot`, because a cross resolved off the L3 read
  needs a handle to the order and its rest time. The row is 72 bytes rather than 64. This is
  shared wire: `quoter-spec` is compiled into velocity, the CLOB and the midpoint quoter, so all
  three `.so` fixtures must be rebuilt together. A one-sided rebuild does not fail as a decode
  error. It presents as broad, unrelated-looking breakage.
- No cross matching and no pricing: R3 lives in velocity.

**velocity (`programs/velocity`)**
- The migration reuses `try_place_remainder_on_clob`, with the taker-origin flag set. It costs no
  extra accounts: a router fill already carries the quoter slab, the book and the clob program,
  because the CLOB baseline is mandatory, so the route adds nothing new.
- `crank_taker_origin_cross` resolves one cross, and it resolves it as an ordinary fill. The
  remainder is the taker of a router pass: the market's baseline book and the routed quoters
  compete on price, `require_baseline` holds the call to carrying the CLOB entry, and the filler
  obligation holds it to the makers it had room for. So the crank sweeps as far into the book as
  the taker's own limit reaches rather than stopping at one counterparty. What is settled is then
  reported back with `fill_v0`, which shrinks the remainder in place.
- **Two taker remainders crossing each other take a second branch of the same crank**: velocity
  computes the match itself and settles the two directly, at the earlier one's price. They cannot be
  resolved through the book. R4's gate withholds a crossed taker-origin order from `execute_v0`, so
  an execute aimed at one would pass over it and fill deeper depth instead, settling against a maker
  the cross was never priced for. Each leg is then reported with `fill_v0`, so both orders shrink in
  place and neither loses its id or its queue position. Without this branch the pair deadlocks:
  each remainder gates the other, so no router pass can reach either one. Because there is no
  `execute_v0` and no router pass in it, this branch is much cheaper than the maker one: about
  43k CU against the maker path's 190k.
- **The cross is found by reading both sides, not by asking for two heads.** The crank and its
  relay resolver each read the L3 of both sides and run `resolve_crosses`, a pure function that
  walks the two ladders and consumes size as it makes matches, so several crossed remainders
  resolve without a fresh read for each and a remainder one level down is visible. `next_cross_v0`
  cannot answer this: it reports only the best order on each side, which is blind to exactly the
  pairs that deadlock. Velocity no longer calls it.
- Price priority still decides which crank owns the front of a book. When neither head is
  taker-origin the front is a maker×maker cross and `crank_cross_match` takes it; clearing it is
  what brings a remainder behind it forward. The two cranks compose rather than racing.

  **The crank enforces this itself, not only the resolver that stages it.** The instruction is
  permissionless, and the cross resolver ranks a taker's improvement ahead of arbitrage, which is
  right once a remainder is at the front and wrong before it is. A hand-built crank aimed at a remainder
  sitting behind a better-priced resting order would fill it out of the depth that order had
  priority on. Both the crank and the resolver read the two heads and refuse when neither
  demands liquidity, so what the resolver stages is exactly what the crank accepts.
- The resolver runs under simulation, so it walks the whole window and hands the crank the depth it
  actually needs to re-find the cross, instead of the crank guessing.
- A partially-consumed remainder stays on the book, still taker-origin and immediately matchable,
  because it has already served its auction window. Removing it would let a cranker delete a
  taker's whole resting order by crossing one unit of it. It keeps its CLOB order id, so a client's
  cancel hint stays valid. A leftover the book would refuse as under its minimum is culled instead,
  and its reservation unwound.
- Crank-fee accounting out of the improvement (`calculate_taker_origin_cross_fee`), and the R5
  invariant. R5 is indifferent to which branch resolved the cross: the reward is drawn from the same
  improvement, capped the same way, and a zero or dust improvement resolves for free either way.

**Relay**
- No new condition or watch: the `CLOB_CRANK_CROSS` slot's resolver
  (`resolve_crank_cross_match`) stages `crank_taker_origin_cross` when it finds a crossed remainder
  and `crank_cross_match` otherwise, at the same `min_payment` (R8).
- The crossing-prefix walk that feeds the arb crank steps over taker-origin nodes, so the two cranks
  compose instead of the arb one being staged for a cross it cannot run.

**SDK / keepers**
- keep-rs stops needing the signed route on its auction/uncross/vAMM paths: once taker remainders
  live on the book, a routed order never rests in a `User.orders` slot, so those paths only ever
  see slot orders, whose route digest is zero. That closes the open item in `notes.md` by removing
  the case rather than plumbing it.

## Decisions

**A. R4 rejects a take on a crossed book rather than uncrossing first.** Uncrossing inside
`execute_v0` would hand back fills outside the prefix the router quoted, and velocity's
quote-to-execute binding rejects those, so it was never on the table. See R4 for the two narrowings that
keep rejecting from costing liveness.

**B. The crank fee sits beside the protocol taker fee rather than inside it.** The reward is a
separate quote debit on the taker, out of the improvement, credited to the cranker's `User` the
way every other perp keeper reward is. The protocol fee schedule is untouched, so the match's own
fee split is identical to any other fill's. That is also why the crank reward does *not* appear as
the fill record's `filler_reward` and gets its own record (`TakerOriginCrossRecordV0`).

**C. Market remainders migrate too.** A market remainder rests behind the same gate and the same
auction as a limit one.
`restable_remainder_price` is the single rule for what may rest, shared by every route, because a
remainder whose fate depends on which route reached it is a bug.

**D. The window has no floor, and zero is a sensible setting.** R7 defers to per-market config,
and zero is the expected default rather than a misconfiguration, because the delay is not what
protects the improvement. R4's reservation is. A remainder claims the depth it crosses whatever
the delay, and claimed depth leaves the book's matchable set for every caller but the crank. At
zero a remainder is takeable at its own price only while nothing crosses it, which is the case
where there is no improvement to take. The moment a counterparty arrives it is claimed and the
cross settles at the counterparty's price.

What a nonzero delay adds is a pre-window in which the order is not matchable at all, so makers can
line up before anyone can trade with it. It also binds the taker: the cancel refusal keys on the
same activation slot, so at zero there is nothing to bind and the owner may pull the order at once.
A market that wants makers to compete before the first fill sets a delay. A market that wants
immediacy sets zero. Both are supported and both are tested.

**E. A reservation lapses if the crank never runs.** A reservation holds depth that only
`crank_taker_origin_cross` can consume, so a turner that never fires would hold the top of the
book until the remainder's `max_ts`. `ClobHeaderV0::reservation_grace_slots` bounds it instead. A
reservation stops being honoured that many slots after its claimant activates, and the depth is
ordinary again. A fresh market starts at 32 slots and `update_market_v0` retunes it, up to a
150-slot ceiling. The window only has to cover the crank transaction, and a transaction cannot
outlive its blockhash. The lapse is read per claimant in the
walk, so one remainder's reservation expiring leaves every other remainder's reservation intact.
Makers keep their own release either way. A cover order can always be cancelled, and the
reservation recomputes without it.

