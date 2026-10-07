# quoter-spec

Wire format for velocity's quoter interface. It is the contract between velocity, the router
program, and every program registered as a quoter: the CLOB, the midpoint, and third-party
PropAMMs.

## Dependents

- `programs/velocity`, the router that calls `quote_v0` and `execute_v0` by CPI.
- `crates/clob-wire` and `crates/clob-state`, which reuse `UserRefV0`, `SideV0` and
  `CancelSidesV0` rather than restate them.
- `crates/quoter-spec-v2`, which compiles this same source for the CLOB and the midpoint. See
  that crate's README for why it is a separate crate.

## Build and test

```
cargo test -p quoter-spec
```

Feature `anchor-derive` adds anchor's borsh derives and `idl-build` adds the `IdlBuild` plumbing
`anchor idl build` needs. Velocity turns both on. The v2 programs turn on neither, so no anchor
code reaches them.

## Design

This crate is the whole interface: the arguments a quoter is called with (`QuoteArgsV0`,
`ExecuteArgsV0`, in `request`), the responses it must produce (`response`), and the writers a
quoter streams an answer with (`write`). Reading it should be enough to implement one.

`quote_v0` and `execute_v0` answer across a program boundary in bytes. Velocity writes the
arguments and reads the responses; the quoter does the reverse. One declaration per program pins
nothing against the other. A field added on one side and forgotten on another gives two
self-consistent programs that disagree about the bytes between them, and the disagreement lands on
a value transfer: a misread `base_size` moves the wrong amount of a user's collateral.

### The quoter-slab signer is shared

Velocity signs `quote_v0` and `execute_v0` as the market's quoter-slab PDA. That key is the same
for every quoter approved on the market, and a CPI callee inherits the signer status of whatever
key it was handed. A quoter therefore holds, live inside its own call, the key velocity
authenticates with at every other quoter on that market.

The key alone does not prove velocity is the immediate caller; another quoter on the same market
could forward it. Velocity closes that path by refusing to approve a quoter whose registered
accounts name another approved quoter's response account, in both directions. A forwarded call can
then never carry the account its callee needs. An implementer keeps any authority gated on this key
on the account the quoter writes its own response to, so no other quoter on the market can name it.
The book stores `place_authority` on its market account, and the midpoint stores
`execute_authority` on its quoter account, each beside the response it writes.

The key is derived per market, so it authenticates nothing at a quoter registered on a different
market.

### Responses are read in place

A response is plain data in the quoter's account, and velocity reads it there: fixed-width
records, little-endian, no length-prefixed nesting, no deserialization step. Velocity's heap is
32 KB and never reclaims, and one fill CPIs every registered quoter twice, once to quote and once
to execute. A response that decoded into `Vec`s would spend heap the fill cannot get back.

Every record is `#[repr(C)]` and free of implicit padding, which both `bytemuck::Pod` and
wincode's zero-copy rules require. A field reordered into a layout with a padding hole stops
compiling instead of changing the wire.

Each section of a response is a count followed by that many fixed-width records, sections
contiguous and in declaration order. A quoter streams a response rather than serializing one,
through `QuoteWriter` and `ExecuteWriter`. An execute response's sections are, in order,
`UserBalanceChangeV0`, `CancelledRemainderV0`, `CompletedOrderV0`, `PartiallyFilledOrderV0`.

### Addresses

Velocity names the address type `Pubkey`; the v2 programs name it `Address`. It is one type:
`solana-pubkey` re-exports `Address as Pubkey`, and `solana-address` 1.x is a ten-line shim over
2.x. It is spelled `Pubkey` here because anchor's IDL derive recognizes the address type by that
token, and velocity is the consumer that runs `anchor idl build`.
