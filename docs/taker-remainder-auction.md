# Taker remainders on the CLOB, and the activation-slot auction

Status: **the CLOB half of R2/R4 is built** (the marker, its report on the removal wire, and the
gate); R1, R3 and R5–R7 are velocity-side and still proposal. Design source of truth for the
surrounding work is the Notion PropAMM doc; this is a focused proposal for one hole in it.

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

**R2 — Taker-origin marker.** A migrated remainder is flagged on the book. `OrderNodeV0.bit_flags`
already exists (`Open`, `Ask`), so this costs no space. The flag means: *this order demands
liquidity; in a cross it is the aggressor.*

**R3 — Cross pricing, in velocity.** When a taker-origin order crosses a resting counterparty, the
match settles at the **counterparty's** price. Best price on the book wins, and price priority
already orders the book that way, so no new selection logic — only the price the match settles at
changes.

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

Enforced in the CLOB's own `execute_v0`, and it **rejects** rather than uncrossing first. Uncrossing
inside `execute_v0` would return fills outside the prefix the router quoted, which velocity's
quote↔execute binding refuses — so "resolve the cross on the way through" is not available to the
book at all.

Narrowed twice, both times to exactly the harm:

- Only a **taker-origin** order is protected. An ordinary maker×maker cross is unclaimed arbitrage,
  not somebody's improvement, and gating on it would freeze every taker on that book.
- Only the order actually being filled is checked, against the best price on the other side that
  could match this slot. So a fill that *consumes the counterparty* still lands — which it must,
  because that is the direction velocity's cross resolution runs: take the counterparty's side with
  `execute_v0` (an ordinary fill at its own price), lift the taker-origin order off with
  `cancel_order_v0`, and settle the pair internally at the counterparty's price. A book-wide gate
  would strand the pair forever.

An order still inside its activation delay — or already expired — is not a counterparty: nothing can
match it, so no improvement is within reach, and gating on it would freeze the remainder for its
whole auction window, which is exactly when it is resting there.

**`quote_v0` publishes only depth `execute_v0` will fill.** The gate would otherwise be a trap: a
taker quotes the book honestly, the router allocates it depth that includes the crossed remainder,
the execute rejects, and the transaction reverts through no fault of the taker's. Velocity binds the
execute to the quoted prefix, so it cannot route around a rejection after the fact — the depth has to
be absent from the quote in the first place.

So quote asks the same predicate execute does, and ends its walk at the first order the gate refuses
rather than skipping past it: execute fails *on* that order, so the depth behind it is not
deliverable either. What quote publishes is the prefix in front of the gated order, which is exactly
what execute can still fill. Sharing the predicate is deliberate — a change to what the gate refuses
that landed on only one of the two would recreate this bug.

The cost is that a gated order shadows the depth behind it on its side until the cross is resolved,
and a taker remainder rests aggressively, so it is usually near the front. That is what execute
failing rather than filling around it buys, and it is bounded by crank latency. If it ever proves too
expensive, the alternative is for execute to *skip* a gated order the way it skips an expired one and
for quote to skip with it — at the cost of making `TakerOriginCrossPending` unreachable.

**R5 — The cranker is paid out of the improvement.** A taker-origin cross only fires when the
improvement exceeds the fee, so it is self-funding and needs no reservoir subsidy on this path.
The fee is capped so the taker's net still beats the price it was resting at — otherwise the
mechanism is worse for the taker than being taken, which is the thing it exists to prevent.

Invariant: `maker_price + taker_fee + crank_fee` must be better for the taker than
`rest_price + taker_fee`. If it is not, do not cross; leave the order resting.

**R6 — Rest price.** For a limit remainder, the order's own limit. For a market remainder,
`auction_end_price` is the only price available, and R1–R5 are what make resting there safe
rather than a free option — the order cannot be picked off before its activation slot, and at
that slot the best resting counterparty wins on price.

**R7 — Activation delay.** The market's `default_activation_delay_slots`, as with any placement.
A market configured at 0 has no auction window: its remainders can only be filled by taking, and
the landing race applies. That is a per-market configuration consequence worth stating in the
admin surface, not a special case in the program.

## What this replaces

`cross_match`'s protocol-as-middleman only makes sense for two *maker* orders crossing, where
neither side is demanding liquidity and the spread is genuinely unclaimed arbitrage. Once
taker-origin crosses price at the maker's side, the protocol's cut disappears from the case that
matters most for user outcomes and remains only for maker×maker crosses. `min_cross_surplus`
keeps floor-guarding those.

## Changes required

**CLOB (`anchor-v2/programs/clob`)** — built.
- `OrderBitFlag::TakerOrigin` (bit 4) plus `PlaceOrderArgsV0::taker_origin` to set it.
- `RemovedOrderV0::taker_origin`, so `cancel_order_v0`/`evict_worst_v0`/`remove_expired_v0` report
  the flag. That is the only place the CLOB reports it, and it is enough: R4 keeps a taker-origin
  cross out of `execute_v0` entirely, so both sides of a cross velocity settles leave the book
  through a removal. The shared quoter-interface types (`UserBalanceChange`,
  `CancelledRemainderV0`) are untouched — every quoter emits those, and only the CLOB can ever
  have an order to mark.
- R4's gate in `execute_v0`, as `ClobError::TakerOriginCrossPending`, and the same predicate in
  `quote_v0`, which ends its walk at the first order the gate would refuse so it never publishes
  depth the fill will reject.
- No cross matching and no pricing: R3 lives in velocity.

**velocity (`programs/velocity`)**
- `fill_perp_order_v1`: the CLOB accounts plus the migration step. Nearly free in accounts — a
  router fill already carries the CLOB entry, its book, the clob program and the quoter signer,
  because the CLOB baseline is mandatory; only `crank_conditions` is new, and it is optional
  everywhere else already.
- The migration itself reuses `try_place_remainder_on_clob`, with the taker-origin flag set.
- The taker-origin cross settles as an ordinary two-user match, not through `cross_match`'s
  protocol pass-through.
- Crank-fee accounting out of the improvement, and the R5 invariant.

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

**B. Where does the crank fee sit relative to the protocol taker fee?** Simplest is a separate
component out of the improvement, paid to the crank payout account, leaving the protocol fee
schedule untouched. The alternative — widening the taker fee on this path and paying the cranker
from it — couples two things that change for different reasons.

**C. Market remainders: migrate now or after this lands?** Migrating them before R1–R5 exist
means resting at a slippage bound with only the activation slot protecting them, which is
strictly worse than today's DLOB behaviour for a market order that would otherwise expire
unfilled. I would hold them.

**D. Minimum viable window.** R7 defers to per-market config, but a market with a 1-slot delay
gives makers no realistic chance to line up. Worth a floor on `default_activation_delay_slots`
for markets that expect taker remainders, or at least a documented recommendation.

## Sequencing

1. `fill_perp_order_v1` migrating **limit** remainders only. No new mechanism: the taker chose
   that price, and being taken at it is what a limit order is. Removes the DLOB from the honest
   case immediately.
2. R2–R5 in the CLOB and velocity: the taker-origin cross.
3. Market remainders migrate once (2) is live.
4. keep-rs drops the signed-route plumbing item, which (1) and (3) make unnecessary.
