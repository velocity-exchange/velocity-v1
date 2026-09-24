# clob-state

The CLOB market account's order-node layout, declared once so an off-chain reader of the same
bytes cannot drift from the book that wrote them.

## Dependents

- `rust/book-publisher`, which decodes a market account with these types to answer which orders a
  user holds, in `user_orders.rs`.
- `fuzz/e2e-svm`.
- `crates/clob-state-v2`, which compiles this same source for the CLOB's own build. See that
  crate's README for why it is separate.

## Build and test

```
cargo test -p clob-state
```

## Design

`crates/clob-wire`'s README states the rule this crate is the exception to: a caller that reads
the market account's bytes reads the book's private memory rather than calling it, and on chain
that is always wrong. An indexer is the case the rule does not cover. It has to answer which orders
a user holds, over every order on the book, at the tick rate of a live feed, and no CLOB
instruction answers that: `orders_v0` describes refs a caller already holds, and `quote_v0` reports
the depth a taker of some size would reach, a different and truncated answer. What is left is the
account itself, and the account is public data an indexer already subscribes to.

So the order-node layout is declared once, here, and the CLOB program uses these same types rather
than its own. An indexer that decodes a node cannot drift from the book that wrote it, because
there is one declaration. This crate must never become a way for another program to read the book:
nothing on chain depends on it. `ORDERS_OFFSET` is the one number that couples the two sides, and
an assertion inside the book's own state module pins it against the book's real header size, so the
book stays free to move anything above it.

Not covered here: the header, the free list, the two sorted lists, and every traversal over them.
A reader that wants the best bid asks the book. A reader that wants every live order walks the
arena, which needs no links at all.
