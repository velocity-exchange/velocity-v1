# Taker remainders on the CLOB, and the activation-slot auction

Status: **built.** R1–R8 are live: the CLOB carries the marker, reports it on the removal wire and
protects a crossed remainder; velocity migrates restable remainders (`fill_perp_order_v1`,
`place_and_take_v1`, `place_and_make_v1`) and resolves the cross with `crank_taker_origin_cross` —
against an ordinary maker, and between two remainders (R3's price-time rule) — and relay turners
discover the crank through the market's existing cross conditions (R8). Signed-message orders and
fired triggers both rest here too, so the auction covers every order that comes to trade and does
not fill. Design source of truth for the surrounding work is the Notion
PropAMM doc; this is a focused proposal for one hole in it.

## The hole

A signed-message taker order cannot be IOC — `place_signed_msg_taker_order` rejects that
explicitly — so anything it does not fill **rests in `user.orders`, on the DLOB**. Nothing
migrates it: the only paths that move a remainder onto the CLOB are `place_and_take_v1` and
`place_and_make_v1`, because they are the only ones holding CLOB accounts. A keeper-driven fill
(`fill_perp_order`) has neither the accounts nor a migration step.

So the rule "if it can rest and be matched, it lives on the CLOB" has an exception nobody
intended, and it is the highest-volume case: every swift order that does not fully fill on
placement.

## Why "someone will just take it at a better price" is not good enough

The tempting answer is to migrate the remainder, let it rest at its own price, and let
counterparties take it. That is how a book normally works, and for an order whose price the user
chose it is exactly right.

It fails for an order resting at a *slippage bound*. A swift market order's only price is
`auction_end_price` — the worst fill it agreed to tolerate. Resting there is writing a free
option: it gets hit precisely when hitting it is profitable for someone else.

And the competition to hit it is the wrong kind. An order becomes matchable at its
`activation_slot`; capturing it by *taking* means landing a transaction in that slot. The winner
is whoever pays the highest priority fee and has the best landing infrastructure, and they take
it at the taker's limit. **Taking is a landing race; the improvement accrues to infrastructure.**

Resting is a price race. A counterparty that wants the order must be on the book *before* the
activation slot, where orders are ordered by price and the outcome is deterministic. Being fast
buys nothing; being cheap wins. That is what the activation slot is for, and it is the mechanism
this proposal leans on.

## Mechanism

The remainder rests on the CLOB with an activation delay. That delay is the auction window:
makers line up inside it, and at the activation slot the best-priced resting counterparty gets
the fill — at **its own price**, so the taker captures the whole improvement.

Concretely, taker's bid rests at 101; makers rest asks at 99 and 100 during the window. At
activation the 99 ask matches: the taker pays 99, the maker gets the 99 it asked for, and the
maker that quoted 100 loses on price rather than on latency.

### Rules

**R1 — Migration.** A fill migrates a restable remainder to the market's CLOB instead of leaving
it on the DLOB. Restable means what it already means on the `place_and_take_v1` path: a fixed
price, no oracle offset, not reduce-only. The rest price and the activation delay are R6 and R7.

Every order that comes to trade and does not fill ends up here. A signed-message order routes at
placement and rests its remainder on the book, so the highest-volume order type in the protocol no
longer rests on the DLOB. A fired trigger rests here too: it is an order that came to trade, so it
gets the counterparty's price in a cross rather than being picked off at its own, and the
taker-origin crank is what carries a route to it.

**R2 — Taker-origin marker.** A migrated remainder is flagged on the book. `OrderNodeV0.bit_flags`
already exists (`Open`, `Ask`), so this costs no space. The flag means: *this order demands
liquidity; in a cross it is the aggressor.*

**R3 — Cross pricing, in velocity.** When a taker-origin order crosses a resting counterparty, the
match settles at the **counterparty's** price. Best price on the book wins, and price priority
already orders the book that way, so no new selection logic — only the price the match settles at
changes.

**When both sides are taker-origin, price-time priority decides which is the counterparty**: the
order that rested first is the maker and its price is the settlement price; the later arrival is the
aggressor and crosses into it. Rest order is the CLOB order id alone — a book's `next_order_id`
only increases and never reuses a value, so the lower id rested first and no separate slot
comparison is needed.

Nothing else about R3 changes, because nothing else needs to: the later order is the one that came
to trade, which is what "taker" means everywhere else in this document, and the earlier order gets
the price it was already offering, which is all a maker is ever promised. The alternative on the
table was the midpoint, and it is worse in the way that matters — it takes half the improvement away
from the order that earned it and hands it to one that was already content, and it makes the
outcome depend on a number neither party quoted.

One consequence worth stating: the *earlier* order captures nothing here — no improvement, no
rebate beyond the ordinary maker rebate. It gets the price it was already offering.

**A partly filled remainder keeps its id and its queue position.** The book API for editing a
resting order's size is `fill_v0`: velocity reports the base it just settled and the order shrinks
in place. The alternative — cancel the order and place a new one for the leftover — settles the
same base at the same price and still charges the taker for it, because the replacement takes a
fresh order id and goes to the back of its price level, behind every order that arrived while the
first one was resting. A taker that waited for its turn would lose it by being filled.

**The CLOB does not do this.** It stays honest: every order fills at its own stored price, exactly
as today, and the book neither reprices a cross nor resolves one. Velocity already computes fill
prices at settlement, so all the CLOB owes it is the taker-origin *fact* — reported on
`RemovedOrderV0` (see R4 for why that is the wire that carries it).

Settling at the counterparty's price is not what `cross_match` does today: it fills each leg at its
own price and hands the difference to the protocol `User` (the surplus `min_cross_surplus` floors).
For a taker-origin leg there is no surplus to hand anywhere — it belongs to the taker. A taker-origin
cross is also simpler than today's crank: an ordinary two-user match at one price, needing none of
`cross_match`'s ephemeral protocol-taker machinery.

**R4 — A crossed taker remainder cannot be taken.** This is load-bearing, not an optimisation.
Without it, an outsider who lands a transaction at the activation slot takes the taker-origin order
at its limit and pockets the improvement — and the landing race is back, with an extra step.

Enforced in the CLOB's own `quote_v0` and `execute_v0`, which **skip** the order: while a
counterparty crosses it, a crossed taker remainder is simply not in the book's matchable set, exactly
as an expired or not-yet-activated order is not. A taker sweeping the side passes over it and fills
whatever is behind it instead.

Narrowed twice, both times to exactly the harm:

- Only a **taker-origin** order is held back. An ordinary maker×maker cross is unclaimed arbitrage,
  not somebody's improvement, and holding either side back over it would cost takers depth for
  nothing.
- Only the order being traded is tested, against the best price on the other side that could match
  this slot. The counterparty itself is never skipped — it is an ordinary maker, and consuming it is
  the fill velocity's cross resolution runs: take the counterparty's side with `execute_v0` (an
  ordinary fill at its own price), settle the pair internally at the counterparty's price, and
  report the settled base back with `fill_v0` so the remainder shrinks in place.

An order still inside its activation delay — or already expired — is not a counterparty: nothing can
match it, so no improvement is within reach, and holding the remainder back then would cost the book
that depth for its whole auction window, which is exactly when it is resting there. A remainder that
nothing crosses is likewise ordinary depth, quotable and takeable at its own price — that is the
fallback when no maker lines up during the window, and how the remainder eventually fills if none
ever does.

**A remainder cannot be cancelled inside its window.** Binding the taker is what makes the auction
an auction: an order its owner can pull the moment a maker lines up offers nothing to line up
against. `Book::cancel` refuses while `slot < activation_slot` for a taker-origin node, and the rule
lives in the CLOB because that is where the flag and the slot are. Liquidation force-cancel passes
`force` and stays exempt, so a distressed account is never blocked, and `max_ts` still bounds the
order's life. The cost is real and worth stating: a taker who signed a market order cannot pull it
for the window, and neither can a delegate.

**Quote and execute skip via one predicate, and that is the point.** A router allocates from the
quote and velocity binds the execute to it, so depth one of them offers and the other withholds is a
reverted transaction for a taker that did nothing wrong — velocity cannot route around a shortfall
after the fact. Both read `book::TakerOriginGate`, so a future change to what the gate withholds
lands on both at once instead of on whichever one someone remembered.

### Why skip rather than fail the call

Three shapes were on the table. **Uncross inside `execute_v0`** is unavailable: it would return fills
outside the prefix the router quoted, which velocity's quote↔execute binding refuses. **Fail the
call** was the first implementation, and it protected the remainder just as well, but it made the
order shadow every level behind it on its side: quote could only publish the prefix in front of it
(anything further was not deliverable, since the fill would hit the remainder and revert), and a
taker remainder rests at a slippage bound, so it normally sits at or near the front. A single crossed
remainder took its whole side dark until the crank resolved the cross.

**Skip** keeps the protection and drops the cliff. The remainder still cannot be taken at its limit;
the depth behind it stays both quotable and fillable, because execute really can deliver it; and
consistency between the two gets easier rather than harder, since they now skip the same order for
the same reason. A taker just finds less depth than it hoped for, which is ordinary book behaviour —
depth vanishes between quote and fill all the time.

The one casualty is `ClobError::TakerOriginCrossPending`, which nothing emits any more. It is
deprecated in place rather than removed: the numeric code is the on-chain identity of every variant
after it.

**R5 — The cranker is paid out of the improvement.** An ordinary filler reward
(`calculate_filler_reward` sizing, so roughly a tenth of the taker fee), charged to the taker in
quote rather than carved out of the taker fee: the improvement belongs to the taker, and widening
the taker fee on this path would couple the crank's cost to the protocol fee schedule, which
changes for unrelated reasons.

Invariant: `maker_price + taker_fee + crank_fee` must be better for the taker than
`rest_price + taker_fee`. The reward is capped by that, and paid **in full or not at all** — an
improvement that cannot cover it resolves the cross for free rather than paying a shaved reward,
which keeps the cranker's revenue predictable and stops the taker funding a keeper subsidy out of a
gain that could not have funded one.

The equal-price cross (and the dust-improvement cross, which is the same case) therefore resolves
rather than resting. "Only fires when the improvement exceeds the fee" reads well until you notice
what refusing costs: the remainder is gated against being taken, so refusing to cross leaves it
with nothing able to clear the gate, and a single unit of dust improvement in front of it would
strand it for its whole life. The taker asked to trade; a fill at a price it already accepted is
what it wanted. The cranker is not working for nothing either way — the market's reservoir pays its
lamports, exactly as it does for every other CLOB crank.

The one hard refusal is a cross that would leave the taker *worse off* than resting, which takes a
taker fee above 100% to reach (the extra fee on the better price is `rate × improvement`).

**R6 — Rest price. Built.** For a limit remainder, the order's own limit. For a market
remainder, `auction_end_price` — its own `price` is zero, and the auction end is the worst fill
it already agreed to. R2–R5 are what make resting there safe rather than a free option: the
order cannot be taken while a counterparty crosses it, and a cross settles at the counterparty's
price, so a maker arriving in the activation window competes on price rather than on transaction
landing. Oracle-offset orders do not migrate — an oracle-floating price has nothing fixed to
rest at.

**The route rides the order's own record, not the book.** A signed-message remainder's quoter list
is stored on `SignedMsgUserOrders` alongside the CLOB order id it rests under, because the route is
velocity's concept and the book has no registry to check it against. The CLOB knows tick, step,
capacity and activation; `taker_origin` earns its place on `OrderNodeV0` because it changes what the
book does, and a route digest changes nothing there. `OrderNodeV0` stays 96 bytes. Off the `Order`
struct the digest is no longer squeezed into five spare bytes either — it is eight, the width the
collision analysis wanted.

A market remainder only exists when the taker's bound is tighter than the vAMM's price: the vAMM
is in every fill's mandatory baseline and quotes deep enough to absorb a market order outright,
so a taker with room to reach the curve simply fills. That is also the case where resting at the
bound matters most — it is the one the taker cannot otherwise complete.

**R7 — Activation delay.** The market's `default_activation_delay_slots`, as with any placement.

A market at 0 is a supported setting, not a broken one. The delay is not what protects the
improvement — R4's gate is, and it keys on whether a live counterparty crosses the order rather than
on the clock. At 0 a remainder is takeable at its own price only while nothing crosses it, which is
exactly when there is no improvement to take.

A nonzero delay buys one thing: a pre-window in which the order is not matchable at all, so makers
can line up before anyone trades with it. It costs one thing: the owner cannot cancel until the
window ends, because the cancel refusal keys on the same activation slot. A market that wants makers
to compete before the first fill pays that cost; a market that wants immediacy sets 0 and the taker
keeps the right to pull its order at any time.

**R8 — Discovery, on the conditions that already exist.** The crank is permissionless, so something
has to notice a resolvable cross; relay turners do, through the market's `CLOB_CRANK_CROSS`
conditions, whose resolver stages `crank_taker_origin_cross` when the top of the matchable book is a
crossed remainder and `crank_cross_match` otherwise. A `ResolvedCrankV0` names its own executor, so
one condition slot serves both.

No new slot and no new watch. A taker-origin cross can newly appear either because a side's best
moved — the cross condition's
8-byte `OnAccountChange` over `best_bid`/`best_ask`, and a crossing order is by definition a new
best — or because a front-of-book order reached its `activation_slot`, which the cross-activation
`AtSlot` hint names precisely (min-folded at every placement, a migrating remainder's included).

The ordering is the economics, not a preference: the improvement belongs to the order that came to
trade, so it is handed over before the protocol middles the same crossed book as arbitrage. It also
keeps the arb crank off a cross it must not run — see "what this replaces" below.

`min_payment` on those conditions stays the market's `keeper_payment_lamports`. That is what relay
measures (`assert_paid_v0` watches the payout account's lamport balance), and this crank pays the
same reservoir lamports as every other. Pricing it above that would make exactly the crosses R5
resolves for free undiscoverable, and a unit of dust in front of a gated remainder is enough to
strand it for its whole life.

The wake that needs care is the **both-remainders** case: two remainders can only face each other
while something crosses the earlier one, so the moment the pair becomes resolvable is usually the
*blocker's* removal rather than a new order's arrival. That moves a head pointer and fires the
change-watch whenever the blocker is its side's head, which is the ordinary case. Two shapes it
misses: a blocker sitting behind a better-priced order nothing can match yet, whose removal rewrites
an arena link and not the head; and a blocker that leaves the matchable set by passing its own
`max_ts`, which is no account write at all. The expire condition's `AtTimestamp` hint covers the
second one hop earlier — it fires at that `max_ts`, and removing the expired order then moves the
head — and the every-slots cross fallback is the floor under both, so a missed hint costs latency
rather than liveness.

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
hands it over at its own resting price with the improvement going to the protocol `User` — the
landing-race outcome R4 exists to prevent, arrived at from the other direction. Stepping over it
restores the composition: the arb crank clears the front of the book, and the remainder's own cross
is what the resolver answers with next.

**A hand-built `crank_cross_match` is held to the same rule.** The instruction is permissionless, so
the walk stepping over a remainder is not enough on its own — the caller controls the account list,
not the walk. `strips_taker_origin_gate` states the exact condition: the book withholds a remainder
only while a live counterparty crosses it, so the cross is unsafe precisely when it consumes that
remainder's entire crossing depth, counting only the rows the legs can actually take. A remainder
nothing crosses has no gate to lose, and cover that is itself a remainder is cover the legs cannot
remove. That narrowness matters — refusing on the mere presence of a crossed remainder would
deadlock the book, because the arb cross in front of a remainder is what clears the way to it.

## Changes required

**CLOB (`anchor-v2/programs/clob`)** — built.
- `OrderBitFlag::TakerOrigin` (bit 4) plus `PlaceOrderArgsV0::taker_origin` to set it.
- `RemovedOrderV0::taker_origin`, so `cancel_order_v0`/`evict_worst_v0`/`remove_expired_v0` report
  the flag. That is the only place the CLOB reports it, and it is enough: R4 keeps a taker-origin
  cross out of `execute_v0` entirely, so both sides of a cross velocity settles leave the book
  through a removal. The shared quoter-interface types (`UserBalanceChangeV0`,
  `CancelledRemainderV0`) are untouched — every quoter emits those, and only the CLOB can ever
  have an order to mark.
- R4's gate as `book::TakerOriginGate`, read by both `quote_v0` and `execute_v0`, which skip a
  crossed taker remainder the way they skip an expired one — so quote never publishes depth the fill
  will not deliver, and the rest of the side stays tradeable.
- `ClobError::TakerOriginCrossPending`, deprecated in place: the gate's first shape failed the call
  instead of skipping, and the numeric code cannot be reused.
- `fill_v0`: velocity reports base it settled against a resting order and the order shrinks in
  place, keeping its id and its queue position. This is what lets a cross resolve without
  cancel-and-replace. It emits the existing `ExecuteRecordV0` — it is a fill, and a consumer that
  already decodes fills needs nothing new.
- `force` on `CancelOrderArgsV0` / `CancelAllArgsV0`, and a `slot` for the window check, so
  liquidation force-cancel can reach a bound remainder and an ordinary cancel cannot.
- `L3RowV0` carries `node_index` and `placed_slot`, because a cross resolved off the L3 read needs
  a handle to the order and its rest time. The row is 72 bytes rather than 64. This is shared wire:
  `quoter-spec` is compiled into velocity, the CLOB and the midpoint quoter, so all three `.so`
  fixtures must be rebuilt together. A one-sided rebuild does not fail as a decode error — it
  presents as broad, unrelated-looking breakage.
- No cross matching and no pricing: R3 lives in velocity.

**velocity (`programs/velocity`)** — built.
- `fill_perp_order_v1`: the CLOB accounts plus the migration step. Nearly free in accounts — a
  router fill already carries the CLOB entry, its book, the clob program and the quoter signer,
  because the CLOB baseline is mandatory; only `crank_conditions` is new, and it is optional
  everywhere else already.
- The migration itself reuses `try_place_remainder_on_clob`, with the taker-origin flag set.
- `crank_taker_origin_cross` resolves one cross, and it resolves it as an ordinary fill. The
  remainder is the taker of a router pass: the market's baseline book and the routed quoters
  compete on price, `require_baseline` holds the call to carrying the CLOB entry, and the filler
  obligation holds it to the makers it had room for. So the crank sweeps as far into the book as
  the taker's own limit reaches rather than stopping at one counterparty. What is settled is then
  reported back with `fill_v0`, which shrinks the remainder in place.
- **Two taker remainders crossing each other take a second branch of the same crank**: velocity
  computes the match itself and settles the two directly, at the earlier one's price. They cannot be
  resolved through the book — R4's gate withholds a crossed taker-origin order from `execute_v0`, so
  an execute aimed at one would pass over it and fill deeper depth instead, settling against a maker
  the cross was never priced for. Each leg is then reported with `fill_v0`, so both orders shrink in
  place and neither loses its id or its queue position. Without this branch the pair deadlocks:
  each remainder gates the other, so no router pass can reach either one. Because there is no
  `execute_v0` and no router pass in it, this branch is much cheaper than the maker one — about
  43k CU against the maker path's ~190k.
- **The cross is found by reading both sides, not by asking for two heads.** The crank and its
  relay resolver each read the L3 of both sides and run `resolve_crosses`, a pure function that
  walks the two ladders and consumes size as it makes matches — so several crossed remainders
  resolve without a fresh read for each, and a remainder one level down is visible. `next_cross_v0`
  cannot answer this: it reports only the best order on each side, which is blind to exactly the
  pairs that deadlock. Velocity no longer calls it.
- Price priority still decides which crank owns the front of a book. When neither head is
  taker-origin the front is a maker×maker cross and `crank_cross_match` takes it; clearing it is
  what brings a remainder behind it forward. The two cranks compose rather than racing.

  **The crank enforces this itself, not only the resolver that stages it.** The instruction is
  permissionless, and the cross resolver ranks a taker's improvement ahead of arbitrage — right
  once a remainder is at the front, wrong before it is. A hand-built crank aimed at a remainder
  sitting behind a better-priced resting order would fill it out of the depth that order had
  priority on. Both the crank and the resolver read the two heads and refuse when neither
  demands liquidity, so what the resolver stages is exactly what the crank accepts.
- The resolver runs under simulation, so it walks the whole window and hands the crank the depth it
  actually needs to re-find the cross, instead of the crank guessing.
- A partially-consumed remainder stays on the book still taker-origin and immediately matchable —
  it has already served its auction window — because removing it would let a cranker delete a
  taker's whole resting order by crossing one unit of it. It keeps its CLOB order id, so a client's
  cancel hint stays valid. A leftover the book would refuse as under its minimum is culled instead,
  and its reservation unwound.
- Crank-fee accounting out of the improvement (`calculate_taker_origin_cross_fee`), and the R5
  invariant. R5 is indifferent to which branch resolved the cross: the reward is drawn from the same
  improvement, capped the same way, and a zero or dust improvement resolves for free either way.

**Relay** — built.
- No new condition or watch: the `CLOB_CRANK_CROSS` slot's resolver
  (`resolve_crank_cross_match`) stages `crank_taker_origin_cross` when it finds a crossed remainder
  and `crank_cross_match` otherwise, at the same `min_payment` (R8).
- The crossing-prefix walk that feeds the arb crank steps over taker-origin nodes, so the two cranks
  compose instead of the arb one being staged for a cross it cannot run.

**SDK / keepers**
- `getFillPerpOrderIx` gains the CLOB accounts (v1 route), mirroring the take builder.
- keep-rs stops needing the signed route on its auction/uncross/vAMM paths: once taker remainders
  live on the book, a routed order never rests on the DLOB, so those paths only ever see
  v0-route orders, whose route digest is zero. That closes the open item in `notes.md` by
  removing the case rather than plumbing it.

## Open questions

**A. R4: reject a take on a crossed book, or uncross first? — settled: reject.** Uncrossing inside
`execute_v0` would hand back fills outside the prefix the router quoted, and velocity's quote↔execute
binding rejects those, so it was never actually on the table. See R4 for the two narrowings that
keep rejecting from costing liveness.

**B. Where does the crank fee sit relative to the protocol taker fee? — settled: beside it.** The
reward is a separate quote debit on the taker, out of the improvement, credited to the cranker's
`User` the way every other perp keeper reward is. The protocol fee schedule is untouched, so the
match's own fee split is identical to any other fill's — which is also why the crank reward does
*not* appear as the fill record's `filler_reward` and gets its own record
(`TakerOriginCrossRecordV0`).

**C. Market remainders: migrate now or after this lands? — settled: they migrate.** R1–R5 are
live, so a market remainder rests behind the same gate and the same auction as a limit one.
`restable_remainder_price` is the single rule for what may rest, shared by every route, because a
remainder whose fate depends on which route reached it is a bug.

**D. Minimum viable window — settled: no floor, and zero is a sensible setting.** R7 defers to
per-market config, and zero is the expected default rather than a misconfiguration, because the
delay is not what protects the improvement. R4's gate is: a remainder is withheld from the book's
matchable set for as long as a live counterparty crosses it, whatever the delay. So at zero a
remainder is takeable at its own price only while nothing crosses it — which is the case where
there is no improvement to take — and the moment a counterparty arrives, the gate closes and the
cross settles at the counterparty's price.

What a nonzero delay adds is a pre-window in which the order is not matchable at all, so makers can
line up before anyone can trade with it. It also binds the taker: the cancel refusal keys on the
same activation slot, so at zero there is nothing to bind and the owner may pull the order at once.
A market that wants makers to compete before the first fill sets a delay; a market that wants
immediacy sets zero. Both are supported and both are tested.

## Sequencing

All four steps are done.

1. ~~`fill_perp_order_v1` migrating **limit** remainders only.~~ Done.
2. ~~R2–R5 in the CLOB and velocity: the taker-origin cross.~~ Done.
3. ~~Market remainders migrate once (2) is live.~~ Done.
4. ~~keep-rs drops the signed-route plumbing item.~~ Done — signed-message orders route and rest
   here, so a routed order never rests on the DLOB and the case is removed rather than plumbed.
