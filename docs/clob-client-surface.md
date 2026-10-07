# CLOB client surface

What velocity-v1 ships so a client can trade the CLOB without reading the book from RPC, without a
new identifier, and without a new history pipeline. All of it is built.

The propamm branch adds 57 instructions and removes two. `place_and_make_perp_order` and
`place_and_make_signed_msg_perp_order` are gone, and `place_and_make_perp_order_v1` replaces them.
Every account change went into reserved padding. Before this work a client could still not trade the
CLOB, for three reasons.

1. A resting CLOB order had no velocity order id, so a client could not name it the way it names
   every other order.
2. Velocity emitted no record when an order was placed, cancelled, evicted or expired on the CLOB,
   so the order-history pipeline never saw it.
3. Nothing served a user their own resting orders. `User.orders` holds none of them. Placement
   reserves the aggregates and writes no slot.

This document covers only velocity-v1. The client work, and the indexer work in infrastructure-v3,
are out of scope.

## The contract this creates

A client gets a resting order the way it used to get one out of `User.orders`.

- One `u32` order id, minted from `User.next_order_id`, unique per user across both venues.
- An `OrderRecord` at placement and an `OrderActionRecord` at every terminal event, on the streams
  the history pipeline already reads.
- An HTTP and websocket feed of a user's own resting orders. The feed carries the CLOB reference
  that cancel and modify need, so no client resolves a reference itself.

## Order identity

Velocity mints the id. Placement takes it from `User.next_order_id`, the same counter that numbers
an account's armed triggers, and passes it to the book.

- `PlaceOrderArgsV0` carries `client_order_id: u32`.
- `OrderNodeV0` carries it in a field of its own. The node is 104 bytes.
- The book echoes it wherever it names an order: `RemovedOrderV0`, `CompletedOrderV0`,
  `CancelledRemainderV0`, and every `…RecordV0` event.
- The book's own `order_id: u64` is unchanged. It orders the book and it breaks placement-slot ties,
  which is a different job from naming an order to a user.

Modify keeps the id. A reprice is one order that moved, not two orders, and a placed trigger forces
it. The shadow slot in `User.orders` keeps the id it armed under, so a new id would leave that slot
naming an order nobody holds. The order does lose its queue position, because the book has no
in-place mutation.

Cancel, modify and force-cancel take `ClobOrderRefV0`. The reference fails closed on a stale hint,
which is the property that makes it safe. The order feed carries it, so a client never has to find
one.

## Records on the pipeline that exists

Velocity emits, from its own instructions, records the history pipeline already decodes. The book's
`…RecordV0` events stay as they are. They are the book's own log, not a second pipeline.

Each record carries a synthesized `Order`: `order_id` is the client order id, `order_type` is Limit,
`post_only` is true, `bit_flags` has `PlacedOnClob` set, plus price, size, direction, `max_ts`,
`market_index` and `slot` from the placement.

| Instruction | Record |
| --- | --- |
| `place_and_make_perp_order_v1`, `trigger_limit_order_v1`, `modify_order_v1` | `OrderRecord` |
| `cancel_order_v1` | `OrderActionRecord`, action Cancel |
| `crank_clob_evict` | `OrderActionRecord`, explanation `ClobOrderEvicted` |
| `crank_clob_remove_expired` | `OrderActionRecord`, explanation `OrderExpired` |
| `force_cancel_clob_orders` | `OrderActionRecord`, explanation `InsufficientFreeCollateral` |
| sub-min cull on the execute wire | `OrderActionRecord`, explanation `ClobRemainderCulled` |

`ClobOrderEvicted` and `ClobRemainderCulled` are appended to the bottom of `OrderActionExplanation`.

`cancel_orders_v1` is the exception, and it has to be. A sweep removes up to 128 orders and an
`OrderActionRecord` is 480 bytes, so per-order records would need 60 KB of logs against a 10 KB
budget. The book's own `OrdersCancelRecordV0` carries the swept ids instead, at about 4 bytes each.
It lists them by the caller's ids, so it joins velocity's stream on the same key without a map. A
reader of order history has to decode that one CLOB event.

`cancel_order_v1` takes a read-only `perp_market` account, for one value: the cached oracle price
its record is stamped with. It is not an oracle account, because a maker pulling orders off a book
must not be able to fail on a stale feed.

Fills already emit `OrderActionRecord` with `OrderFilledWithExternalQuoter`. Two fields are missing
on the maker half, and both are filled from the wire.

- `maker_order_id` and `maker_order_direction` come from the client order id the book echoes on
  each consumed order, when the balance change names exactly one order
  (`ExecuteResponseV0::sole_client_order_id`). A change merges every order of one maker, so a sweep
  that took three of someone's orders names none of them and the field stays absent.
- A partially filled order is reported by `PartiallyFilledOrderV0` on `ExecuteResponseV0`. A
  best-first sweep leaves at most one partial per book per execute, so this is one record, not a
  list. Without it, a small fill against one resting order has no order to attribute to at all.
- The order's size fields stay absent on a book maker's fill record. Velocity holds no per-order
  state for a book order, so it cannot state the order's original size or its running totals. A
  record that reported this fill's size as the order's size would be wrong. A reader has both from
  the place record.

## The order feed

Book production already runs in Rust and already subscribes to the whole CLOB program.
`rust/book-publisher` carries a per-user index beside the books it writes.

### Shared layout crate

`OrderNodeV0`, `NODE_BYTES` and the node bit flags live in `crates/clob-state`, which the program
and the publisher both depend on. The header stays in the program. What crosses is one number,
`ORDERS_OFFSET`, which the book asserts against the shared crate at compile time. A header that
grows fails the book's build rather than moving the arena under a reader.

Generating this from an IDL does not work. anchor-v2's `Slab<H, T>` IDL impl forwards to `H` only
(`lang-v2/src/accounts/slab.rs`), so a generated IDL describes the market account as its header and
says nothing about the node arena. There is no `OrderNodeV0` in it, and no offset, which is a
derived number no IDL states. The CLOB's `idl-build` is an empty stub besides. Host-building the
program crate from the `rust/` workspace is the other option, and it drags the anchor-v2 alpha
dependency tree across a workspace boundary.

### Publisher

Per tick, per market: walk the arena, keep live nodes, group by `UserRefV0`, derive the velocity
`User` PDA. Write one key per user per market (`last_update_user_orders_{userPda}_{marketIndex}`)
and publish the same payload on `user_orders_{userPda}`.

Two gates keep this quiet, cheapest first. The market's whole arena is fingerprinted, and an
unchanged arena ends the tick before anything is decoded. That is almost every tick, because prices
move continuously and resting orders do not. Past that, each user's own rows are fingerprinted, and
only the users whose rows moved are written. A user who had orders and now has none still gets one
write, an empty list, because a subscriber that heard nothing cannot tell an empty book from a quiet
one. The key is deleted in the same step, so a reader starting fresh finds nothing rather than a
stale empty document.

The publisher writes one key per market rather than a hash keyed by market. The serving side's Redis
wrapper exposes no hash commands and does expose `mget`, and the market set is small and known at
both ends.

One order carries: `orderId` (the client order id), `clobOrderId`, `nodeIndex`, `marketIndex`,
`direction`, `price`, `baseAssetAmount` remaining, `maxTs`, `activationSlot`, `placedSlot`,
`takerOrigin`, and a `venue` tag.

The `venue` tag says where an order lives, and so which cancel path it takes. Every live order
rests on the book now; what is left in `User.orders` is unfired conditionals, which a client shows
as armed triggers rather than as resting depth.

### Server

`apps/dlob-server` serves `GET /userOrders?userPubkey=` from those keys, and a `user_orders`
websocket channel that maps to the pub/sub channel above. The channel router already maps a
subscribe message to a Redis channel, so this is one case in that switch. Resting orders are public
on-chain state, so the endpoint needs no auth.

## SDK surface

- `placeAndMakePerpOrder`, `cancelOrderV1`, `modifyOrderV1` and `cancelOrdersV1` on
  `VelocityClient`. Cancel and modify resolve `quoterSlab`, `clobMarket` and `clobProgram` from the
  market's quoter slab, so a caller passes the market index and the order's handle.
- Cancel and modify take the `ClobOrderRefV0` from the feed, which already holds `nodeIndex`. A
  caller re-resolves once and retries on a stale-hint failure, because a fill can move a node
  between read and send.
- `liquiditySource` in `dlob/orderBookLevels.ts` carries `'clob'` and `'propamm'`.
- `UserClobOrdersClient` reads the HTTP and websocket endpoints and returns the feed shape.
- `SignedMsgOrderParamsMessage.network` is set by default in the swift helpers. It is optional
  on-chain and the pre-tag encoding still decodes, but an untagged devnet order replays on mainnet.
- Every new type is mirrored in `types.ts`, with a changeset and the §4 and §6 rows in
  `DRIFT-TO-VELOCITY.md`.

## Post-only

Post-only needed no fee work. A resting CLOB order settles at its own price on the match-fee
schedule in every path that can consume it. A router taker fills it through
`settle_external_match_fill`, and a maker-versus-maker cross runs the protocol `User` as the taker on
both legs, which is who pays the taker fees. A post-only order is never taker-origin, so it never
becomes the aggressor in a cross. `MUST_POST_ONLY` is satisfied by construction.

What was missing is the placement ergonomic, and it ships as `reject_if_crossed` on the book's
`PlaceOrderArgsV0`. An order that would cross the opposite best is refused rather than rested
crossed. `place_and_make_perp_order_v1` sets the flag from its `post_only` param, and
`modify_order_v1` takes the flag directly. The check measures against the opposite best whatever its
state. An order still inside its activation delay is resting liquidity a moment from now, and a
caller asking not to cross does not want to cross that either.

A refused modify leaves the maker with no order, because the original is already off the book when
the replacement is refused. That is what a maker repricing into a crossed book is asking for.

An oracle-offset limit is refused outright (`InvalidOrderOracleOffset`): it cannot rest at a fixed
price on a book, and there is no longer anywhere else for it to rest. A maker that wants an
oracle-relative quote runs a PropAMM quoter. A reduce-only limit does rest on the book; velocity
counts those on `PerpPosition.reduce_only_clob_orders` and caps that user's reduce-only fills to the
position they reduce, which is the state the book does not hold.

## Tests

- `integration-tests/router_fill.rs`: place through velocity and assert the id minted from
  `User.next_order_id` round-trips onto the book and back out of the removal wire, and that the
  emitted `OrderRecord` and `OrderActionRecord` name the order by it. Events are decoded out of the
  transaction's logs, so this reads what a subscriber reads.
- `anchor-v2/programs/clob`: the partial-fill record exists for every size that stops mid-order and
  never twice. A placement refuses to rest crossed when asked, at the touch and through it, and
  rests when it did not ask.
- `crates/clob-state`: a live order reports its arena index rather than its position among live
  orders. The index is half a cancel hint, so counting live orders would hand a caller a hint
  pointing at another order. A short or partly written account yields nothing rather than failing.
- `rust/book-publisher`: the two gates. An unchanged arena writes nothing, and one user placing does
  not republish anyone else's rows. Emptying publishes once and then goes quiet. Markets are gated
  independently.
- `packages/sdk`: the feed's wire form deserializes, keeping velocity's ids and the book's handles
  apart.

One fixture pins the row shape and both sides assert against it
(`packages/sdk/tests/fixtures/userClobOrderRow.json`). The publisher asserts that it emits exactly
that, and the SDK asserts that it reads exactly that. A producer test and a consumer test that each
build their own fixture are blind to the wire between them. A field the producer stops emitting
stays in the consumer's literal and both suites pass. That is the argument `quoter-spec` and
`clob-wire` make for the on-chain wires, applied to a JSON one.

## Still to do

Three things this branch does not close. None of them blocks a client from being built against the
surface. All three are worth knowing before one is.

### The CLOB's events have no TypeScript decoder, so order history is incomplete

Two facts live only on the book's own event stream. `OrdersCancelRecordV0` holds the per-order detail
of a sweep, which cannot be a velocity record because 128 orders do not fit a 10 KB log budget.
`ExecuteRecordV0` holds per-order fill sizes, which the response wire does not carry per order. Both
name orders by velocity's ids, so they join the existing stream on the same key rather than needing
a pipeline of their own. What is missing is the decoder, and the reason is upstream: anchor-v2 emits
no IDL for the CLOB. `idl-build` is an empty stub, and even a complete IDL would describe the market
account as its header, because `Slab<H, T>` forwards only `H`.

Until the decoder exists, a maker's own fills attribute to an order only when the fill's balance
change named exactly one, and a cancel-all shows as a set of orders that stopped without a per-order
reason. The work lands in infrastructure-v3, not here. Getting anchor-v2 to emit an IDL is the fix
worth pushing. A hand-written decoder is the fallback.

### The feed has never run against a live Redis and publisher

Both sides are unit-tested and the row shape is pinned across them, but nothing exercises the whole
path end to end: publisher tick, Redis write, HTTP read, websocket fan-out. `test:e2e:localnet` does
not cover it. The gates that make the feed cheap, such as an unchanged arena writing nothing, are
the kind of thing that is correct in a unit test and wrong against a real account feed. Run the path
by hand before a client is built on it.

### The webapp is several SDK versions behind, independent of any of this

It pins `@velocity-exchange/sdk` 0.4.0 against 0.24.0 here. Equity floor, isolated positions,
revenue share and the swap clients all have to be absorbed regardless. Scope that catch-up on its
own rather than counting it as part of adopting the CLOB.

## Not in this plan

Positions, margin and balances still come from a `User` account subscription over RPC. The seam for
moving them to an API is the one the order feed uses. A client that reads orders from an API and
positions from RPC sees the two halves of a fill at different times. That is a real cost and it is
worth pricing separately.
