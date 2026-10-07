# clob-wire

The CLOB's instruction arguments and return data: the bytes velocity writes and the book reads,
declared once so neither program can drift from the other's field widths or order.

## Dependents

- `programs/velocity`, which calls the CLOB by CPI using these types for every instruction
  argument and return value.
- `crates/clob-wire-v2`, which compiles this same source for the CLOB's own build. See that
  crate's README for why it is separate.

## Build and test

```
cargo test -p clob-wire
```

Feature `anchor-derive` adds anchor's borsh derives and `idl-build` adds the IDL plumbing, both
needed only by velocity's build.

## Design

`crates/quoter-spec` makes the case for one declaration per program boundary: two declarations pin
nothing against each other, and a field reordered on one side gives two self-consistent programs
that disagree about the bytes between them. The disagreement lands on a placement, where a misread
`base_asset_amount` rests the wrong size against a real user's margin. This crate is that
declaration for the surface the CLOB has beyond the quoter interface: place, cancel, evict, and
reclaim an expired order. Every PropAMM implements `quoter-spec`; these instructions are not part
of that interface, because only velocity's router and the CLOB speak them.

Not covered here: the book's account layout, meaning its header offsets and node arena
(`crates/clob-state`), and the quoter interface itself (`crates/quoter-spec`).

Serialization is per-consumer, as in `quoter-spec`. Velocity encodes with anchor's borsh and needs
the IDL plumbing; the book writes the bytes itself and carries no borsh crate, for its binary size
and compute budget. The two encodings are byte-compatible, because wincode's configuration here is
anchor's own borsh config, and `programs/velocity/src/state/prop_amm/tests.rs` pins that agreement
for every type in this crate.

See [`docs/taker-remainder-auction.md`](../../docs/taker-remainder-auction.md) for the rules an
unfilled taker remainder follows once it rests on the book through these types.
