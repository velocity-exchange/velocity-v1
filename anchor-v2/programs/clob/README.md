# CLOB

Velocity's on-chain order book, `BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU`. The book rests
orders in price-time priority and is one of the quoters `programs/velocity`'s router can reach by
CPI. It builds against an alpha branch of anchor's next major version rather than the official
`anchor-lang` velocity uses; see `crates/quoter-spec-v2`'s README for why.

## Dependents

Velocity is the only caller in production. `fuzz/e2e-svm` and `fuzz/e2e-svm-revshare` load the
built `.so` as a fixture for cross-program fuzzing.

## Build and test

```
bun run program:build:clob    # cargo-build-sbf, arch v3
bun run test:clob             # builds, then cargo test -p clob
```

`cargo test -p clob` alone also works once the `.so` in `target/deploy/` is current; several tests
load it directly through litesvm.

## Instructions

Gated on `place_authority` (velocity's quoter CPI signer PDA; only velocity calls these):
`place_order_v0`, `cancel_order_v0`, `cancel_all_v0`, `evict_worst_v0`, `remove_expired_v0`,
`execute_v0`, `fill_v0`. `execute_v0` and `fill_v0` share this gate because only velocity settles
a fill: `execute_v0` is the book filling its own orders for a taker it can see, and `fill_v0`
reports a fill velocity made against a taker remainder resting here, matched against liquidity
the book cannot see.

Read-only, open to any caller for simulation: `next_removal_v0`, `order_rules_v0`, `orders_v0`,
`next_cross_v0`, `quote_v0`, `quote_l3_v0`.

Market admin: `initialize_market_v0`, `close_market_v0`, `update_market_v0`, `resize_market_v0`,
`set_crank_conditions_v0` (registers the resolvers for this book's own relay crank conditions).

## Design

`src/state.rs` holds the account layout and the wire types; `src/book.rs` holds the order-book
algorithm and the streaming encoder that writes quote and execute payloads into the market's
response region; `src/emit.rs` holds the allocation-free event log path. `crates/clob-wire`
declares the instruction arguments and return data velocity and the book must agree on bit for
bit, and `crates/clob-state` declares the order-node layout an off-chain indexer decodes from the
same account. See [`docs/taker-remainder-auction.md`](../../../docs/taker-remainder-auction.md)
for the rules a resting taker remainder follows, including why `fill_v0` shrinks an order in place
instead of cancelling and re-placing it.
