# quoter-health (`velocity-quoter-health`)

Quoter health tracking for an off-chain router. A library crate; it has no binary of its own.

## Dependents

- `rust/router-sim`, which reads its scores when it decides which registry entries a simulated
  fill's transaction should carry.
- `rust/book-publisher`, which reports the same evidence and serves it on `/metrics`.
- `rust/keep-rs`, `rust/swift`.

## Build and test

```
cargo check --manifest-path rust/Cargo.toml -p velocity-quoter-health
cargo test --manifest-path rust/Cargo.toml -p velocity-quoter-health
```

Feature `redis-store` pools observations and pins across processes; it is off by default so a
single router runs with no external state.

## Design

PropAMM quoters are arbitrary programs the on-chain router invokes by CPI. A quoter can revert,
answer off its own quote, consume the whole compute budget, or offer depth it cannot carry, and
the on-chain router cannot refuse any of that: it quotes whichever registry entries the
transaction carries, and one failing entry reverts the whole fill.

The off-chain router chooses which entries the transaction carries, so leaving a quoter out is
enough to avoid it. This crate holds the evidence that choice needs, and the state machine that
lets the choice reverse.

Observation starts at the simulate call, not on chain: a router simulates before it sends, so a
quoter that reverts fails the simulation and the transaction never lands, with no log archived and
no counter moved except in the process that held the simulate call.

A quoter is charged only when the evidence names it. A simulation carries several quoters and can
fail for reasons that belong to none of them, so an unproven failure counts against the router
instead; a rising unattributed rate means attribution has a hole, not that a maker got worse.
Velocity names the entry whenever it refuses an answer. A quoter that never answers ends
velocity's instruction before it can log, so only the runtime's CPI frame survives, naming a
program rather than an entry; a re-simulation without that suspect turns the suspicion into proof.

Every automatic exclusion expires, so degradation reverses on its own. An operator's pin lives in a
layer the scorer cannot write, so clearing a pin restores automatic behavior with nothing lost.

Deliberately no chain access: routers hand this crate the outcome of a simulation they already
ran, so it never needs a `ChainSource` and never pulls the simulation layer into a caller that
does not want it.
