# quoter-emit-v2

Stack-buffer event emission for the anchor v2 quoter programs: the same bytes anchor's
`Event::data()` returns, built without a heap allocation.

## Dependents

- `anchor-v2/programs/clob`
- `anchor-v2/programs/midpoint`

## Build and test

This crate is excluded from the root Cargo workspace (`crates/quoter-emit-v2` in its
`exclude` list) because it depends on the anchor v2 fork unconditionally, and only the
`anchor-v2` workspace builds it:

```
bun run program:build:clob
bun run program:build:midpoint
```

## Design

Anchor's `emit!` macro goes through `Event::data()`, which returns a `Vec<u8>` on both of anchor
v2's event flavors; the bytemuck flavor allocates a buffer only to copy the struct into it. What
reaches the runtime is `[discriminator][body]` handed to `sol_log_data` as one field, so
`emit_pod!` builds that on the stack and calls the syscall directly.

The call passes one field, not two. `sol_log_data` base64-encodes each slice it receives into a
separate entry of a space-separated list, and decoders such as velocity's `EventSubscriber` and
the TypeScript SDK base64-decode the whole `Program data:` line as one blob. The discriminator and
the body must therefore be contiguous.

The bytes `emit_pod!` emits are identical to `Event::data()`'s bytes; that is the contract with
every decoder. `assert_pod_matches_event!` pins one program's record against the trait
implementation so a change to either side fails the build instead of drifting silently.

A variable-length record needs a streaming encoder rather than this copy. The CLOB owns the two
such records and keeps its own writer for them, in `anchor-v2/programs/clob/src/emit.rs`.
