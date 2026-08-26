# CLOB client surface

Status: **built.** W1-W5 are live. What velocity-v1 ships so a client can trade the CLOB without
reading the book from RPC, and without a new identifier or a new history pipeline.

The propamm branch is additively compatible: 52 instructions added, none removed, and every account
change went into reserved padding. A client keeps working after the upgrade. It cannot *use* the
CLOB, because three things are missing on this side of the boundary.

1. A resting CLOB order has no velocity order id, so a client cannot name it the way it names every
   other order.
2. Velocity emits no record when an order is placed, cancelled, evicted or expired on the CLOB, so
   the order-history pipeline never sees it.
3. Nothing serves a user their own resting orders. `User.orders` holds none of them —
   `place_clob_order` reserves the aggregates and writes no slot.

This document covers only velocity-v1. The client work, and the indexer work in infrastructure-v3,
are out of scope.

## The contract this creates

A client gets a resting order the same way it gets a DLOB order today:

- one `u32` order id, minted from `User.next_order_id`, unique per user across both venues;
- an `OrderRecord` at placement and an `OrderActionRecord` at every terminal event, on the streams
  the history pipeline already reads;
- an HTTP + websocket feed of its own resting orders, carrying the CLOB reference that cancel and
  modify need, so no client ever resolves a reference itself.

## W1 — Order identity

Velocity mints the id. `place_clob_order` takes it from `User.next_order_id`, the same counter that
numbers DLOB orders, and passes it to the book.

- `PlaceOrderArgsV0` gains `client_order_id: u32`.
- `OrderNodeV0` stores it in `padding: [u8; 4]`. The node stays 96 bytes.
- The book echoes it wherever it names an order: `RemovedOrderV0`, `CompletedOrderV0` (in `_pad`),
  `CancelledRemainderV0`, and every `…RecordV0` event.
- The book's own `order_id: u64` is unchanged. It orders the book and it breaks placement-slot ties,
  which is a different job from naming an order to a user.

Modify keeps the id. A reprice is one order that moved, not two orders, and a placed trigger forces
it: the shadow slot in `User.orders` keeps the id it armed under, so a new id would leave that slot
naming an order nobody holds. The order does lose its queue position, because the book has no
in-place mutation.

Cancel, modify and force-cancel keep taking `ClobOrderRefV0`. The reference fails closed on a stale
hint, which is the property that makes it safe; the order feed (W3) carries it, so a client never
has to find one.

## W2 — Records on the pipeline that exists

Velocity emits, from its own instructions, records the history pipeline already decodes. The book's
`…RecordV0` events stay as they are — they are the book's own log, not a second pipeline.

Each record carries a synthesized `Order`: `order_id` = the client order id, `order_type` = Limit,
`post_only` = true, `bit_flags` with `PlacedOnClob` set, plus price, size, direction, `max_ts`,
`market_index` and `slot` from the placement.

| Instruction | Record |
| --- | --- |
| `place_clob_order`, `trigger_clob_order`, `modify_clob_order` | `OrderRecord` |
| `cancel_clob_order`, `cancel_all_clob_orders` | `OrderActionRecord`, action Cancel |
| `crank_clob_evict` | `OrderActionRecord`, explanation `ClobOrderEvicted` |
| `crank_clob_remove_expired` | `OrderActionRecord`, explanation `OrderExpired` |
| `force_cancel_clob_orders` | `OrderActionRecord`, explanation `InsufficientFreeCollateral` |
| sub-min cull on the execute wire | `OrderActionRecord`, explanation `ClobRemainderCulled` |

`ClobOrderEvicted` and `ClobRemainderCulled` append to the bottom of `OrderActionExplanation`.

`cancelAllClobOrders` is the exception, and it has to be. A sweep removes up to 128 orders and an
`OrderActionRecord` is 480 bytes, so per-order records would need 60 KB of logs against a 10 KB
budget. The book's own `OrdersCancelRecordV0` carries the swept ids instead, in ~4 bytes each — and
it now lists them by the *caller's* ids, so it joins velocity's stream on the same key without a
map. A reader of order history has to decode that one CLOB event.

`cancelClobOrder` gained a read-only `perp_market` account, for one thing: the cached oracle price
its record is stamped with. Deliberately not an oracle account — a maker pulling orders off a book
must not be able to fail on a stale feed.

Fills already emit `OrderActionRecord` with `OrderFilledWithExternalQuoter`. Two fields are missing
on the maker half, and both are filled from the wire:

- `maker_order_id` and `maker_order_direction` come from the client order id the book echoes on
  each consumed order, when the balance change names exactly one order
  (`ExecuteResponseV0::sole_client_order_id`). A change merges every order of one maker, so a sweep
  that took three of someone's orders names none of them and the field stays absent.
- A partially filled order is reported by a new `PartiallyFilledOrderV0` on `ExecuteResponseV0`.
  A best-first sweep leaves at most one partial per book per execute, so this is one record, not a
  list. Without it the common case — a small fill against one resting order — has no order to
  attribute to at all.
- The order's *size* fields stay absent on a book maker's fill record. Velocity holds no per-order
  state for a book order, so it cannot state the order's original size or its running totals, and a
  record that reported this fill's size as the order's size would be wrong. A reader has both from
  the place record.

`PlacedOnClob` currently documents itself as a property of a `User` slot. Widen the doc comment: it
marks an order that rests on the CLOB, in a slot or in a record.

## W3 — The order feed

Book production already runs in Rust and already subscribes to the whole CLOB program.
`rust/book-publisher` gains a per-user index beside the books it writes.

**Shared layout crate.** `OrderNodeV0`, `NODE_BYTES` and the node bit flags move out of
`anchor-v2/programs/clob/src/state.rs` into `crates/clob-state`, which the program and the publisher
both depend on. The header stays in the program; what crosses is one number, `ORDERS_OFFSET`, which
the book asserts against the shared crate at compile time — so a header that grows fails the book's
build rather than silently moving the arena under a reader.

Generating this from an IDL does not work, and the reason is worth recording: anchor-v2's
`Slab<H, T>` IDL impl forwards to `H` only (`lang-v2/src/accounts/slab.rs`), so a generated IDL
describes the market account as its header and says nothing about the node arena — no
`OrderNodeV0`, and no offset, which is a derived number no IDL states. The CLOB's `idl-build` is an
empty stub today besides. Host-building the program crate from the `rust/` workspace is the other
option, and it drags the anchor-v2 alpha dependency tree across a workspace boundary.

**Publisher.** Per tick, per market: walk the arena, keep live nodes, group by `UserRefV0`, derive
the velocity `User` PDA. Write one key per user per market
(`last_update_user_orders_{userPda}_{marketIndex}`) and publish the same payload on
`user_orders_{userPda}`.

Two gates keep this quiet, cheapest first. The market's whole arena is fingerprinted, and an
unchanged arena ends the tick before anything is decoded — which is almost every tick, because
prices move continuously and resting orders do not. Past that, each user's own rows are
fingerprinted and only the users whose rows moved are written. A user who *had* orders and now has
none still gets one write, an empty list, because a subscriber that heard nothing cannot tell an
empty book from a quiet one; the key is deleted in the same step, so a reader starting fresh finds
nothing rather than a stale empty document.

One key per market rather than a hash keyed by market: the serving side's Redis wrapper exposes no
hash commands and does expose `mget`, and the market set is small and known at both ends.

One order carries: `orderId` (the client order id), `clobOrderId`, `nodeIndex`, `marketIndex`,
`direction`, `price`, `baseAssetAmount` remaining, `maxTs`, `activationSlot`, `placedSlot`,
`takerOrigin`, and a `venue` tag.

`venue` is load-bearing. Oracle-offset limits and reduce-only limits stay on the DLOB, so a user's
open orders come from two places for as long as the DLOB lives. The client renders one list, and
the tag is what tells it which cancel path an order takes.

**Server.** `apps/dlob-server` gains `GET /userOrders?userPubkey=` reading the hash, and a
`user_orders` websocket channel that maps to the pub/sub channel above. The channel router already
maps a subscribe message to a Redis channel, so this is one case in that switch. Resting orders are
public on-chain state, so the endpoint needs no auth.

## W4 — SDK surface

- `placeClobOrder`, `cancelClobOrder`, `modifyClobOrder`, `cancelAllClobOrders` on
  `VelocityClient`, each resolving `quoter`, `clobMarket`, `clobProgram`, `quoterSigner` and
  `crankConditions` from `PerpMarket.clob_quoter` and the PDAs. A caller passes an order and a
  market, nothing else.
- Cancel and modify take the order object from the feed, which already holds `nodeIndex`. Re-resolve
  once and retry on a stale-hint failure, because a fill can move a node between read and send.
- `liquiditySource` in `dlob/orderBookLevels.ts` gains `'clob'` and `'propamm'`.
- A `UserOrdersClient` over the HTTP and websocket endpoints, returning the feed shape.
- `SignedMsgOrderParamsMessage.network` set by default in the swift helpers. It is optional on-chain
  and the pre-tag encoding still decodes, but an untagged devnet order replays on mainnet.
- Mirror every new type in `types.ts`, add a changeset, and add the §4 and §6 rows to
  `DRIFT-TO-VELOCITY.md`.

## W5 — Post-only, and what stays on the DLOB

Post-only needed no fee work. A resting CLOB order settles at its own price on the match-fee
schedule in every path that can consume it: a router taker fills it through
`settle_external_match_fill`, and a maker-versus-maker cross runs the protocol `User` as the taker
on both legs, which is who pays the taker fees. A post-only order is never taker-origin, so it never
becomes the aggressor in a cross. `MUST_POST_ONLY` is satisfied by construction.

What was missing is the placement ergonomic, and it ships as `reject_if_crossed` on
`place_clob_order` and `modify_clob_order`: an order that would cross the opposite best is refused
rather than rested crossed. Measured against the opposite best whatever its state — an order still
inside its activation delay is resting liquidity a moment from now, and a caller asking not to cross
does not want to cross that either.

A refused *modify* leaves the maker with no order, because the original is already off the book when
the replacement is refused. That is what a maker repricing into a crossed book is asking for.

Oracle-offset limits and reduce-only limits stay on the DLOB. Both are enforced at placement against
state the book does not hold. W3's `venue` tag is what makes this invisible to a client.

## Sequencing

1. `client_order_id` through the wire and the node, and `crates/clob-state`. Everything else
   depends on the id existing.
2. W2 records.
3. W3 publisher index and server endpoints.
4. W4 SDK.
5. W5 alongside W1, since both change the placement args.

## Tests

- `integration-tests/`: place through velocity, assert the client order id round-trips onto the node,
  onto every removal wire, and onto the emitted `OrderRecord`; a partial fill attributes to a maker
  order id; evict, expire, cull and force-cancel each emit their explanation.
- `rust/book-publisher`: the arena walk against a synthesized book, grouping and the unchanged-skip.
- SDK: unit tests for the account resolution and the stale-hint retry.

## Left for the indexer

Order history in infrastructure-v3 needs two things this side cannot do for it: decode the CLOB's
`OrdersCancelRecordV0` (the only per-order detail a sweep produces) and `ExecuteRecordV0` (per-order
fill sizes, which the response wire deliberately does not carry per order). Both now name orders by
velocity's ids, so they join the existing stream on the same key. Neither has a TypeScript decoder
today, because anchor-v2 emits no IDL for the CLOB — that is the thing worth fixing upstream.

## Not in this plan

Positions, margin and balances still come from a `User` account subscription over RPC. The seam for
moving them to an API is the same one the order feed uses, and a client that reads orders from an
API and positions from RPC sees the two halves of a fill at different times. That is a real cost and
it is worth pricing separately.
