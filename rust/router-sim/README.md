# router-sim (`velocity-router-sim`)

Quotes a perp market's router liquidity by simulating the fill. A library crate; it has no binary
of its own.

## Dependents

- `rust/book-publisher`, which uses this crate's `quote_view` and `health` modules to price every
  book source (vAMM, resting orders, CLOB, PropAMMs) for publication.
- `rust/keep-rs`, `rust/swift`.

## Build and test

```
cargo check --manifest-path rust/Cargo.toml -p velocity-router-sim
cargo test --manifest-path rust/Cargo.toml -p velocity-router-sim
```

## Design

The router splits a taker across the vAMM, resting DLOB orders, the CLOB, and any registered
PropAMM. It learns each external quoter's prices by CPI into `quote_v0`. A Custom quoter is an
arbitrary third-party program, so there is no general way to decode its book from outside; a call
is the only way to price it, which makes simulation the only correct approach, and it covers the
CLOB and the vAMM as well. This crate does not decode books off chain.

`quote_view` builds the real `fill_perp_order` transaction, runs it against cached chain state in
an in-process SVM (`relay_chain_source`), and reads the answer out of logs, return data, and
post-simulation account state. The result is the split the on-chain code produces, including the
margin clamps, the at-or-better rejections, the mandatory CLOB baseline check, and the compute
unit cost.

`health` chooses which registry entries a simulated fill's transaction carries, reading
`rust/quoter-health`'s scores and retrying a simulation with a suspect quoter excluded to turn a
suspicion into proof.

The account feed keeps a quote cheap. Subscribing to the CLOB program, the per-market quoter
slabs, and each live PropAMM through an unfiltered `relay_chain_source::ProgramSubscription` keeps
every account a fill touches resident, so a quote costs microseconds and no RPC call.
