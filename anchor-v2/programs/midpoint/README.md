# Midpoint

Velocity's midpoint (spline) quoter program, `eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D`. It
builds against an alpha branch of anchor's next major version rather than the official
`anchor-lang` velocity uses; see `crates/quoter-spec-v2`'s README for why.

The program is multi-tenant. A maker does not deploy a quoter program; a maker creates a
`MidpointQuoterV0` PDA instance of this one and registers it as a Custom quoter in velocity's
registry. Each instance is a spline around a midpoint: a per-side ladder of
`(offset-from-mid, size)` levels that change rarely, plus a mid price the maker's hot key tracks
tick by tick through `set_mid_v0`.

## Dependents

Velocity is the only caller.

## Build and test

```
bun run program:build:midpoint    # cargo-build-sbf, arch v3
bun run test:midpoint             # builds, then cargo test -p midpoint
```

## Instructions

Reached by velocity through the quoter interface: `quote_v0` (read-only) and `execute_v0`, gated
on the registered `execute_authority`, velocity's quoter CPI signer PDA. Velocity clamps size to
the quoted user's margin before it calls here.

Maker-operated: `initialize_quoter_v0`, `update_quoter_v0`, `set_levels_v0`, `cancel_all_v0`, and
the hot-path `set_mid_v0`. `propose_authority_v0` / `accept_authority_v0` rotate the instance's
`authority` key.

## Design

Each instance holds two authorities that do not derive from each other: the maker's config key,
`authority`, and the quoted velocity `User`'s wallet, `user_authority`, which signs creation and
seeds the PDA. No trust-bearing value is configured locally; the protected-flow gate reads
`taker_served_window` off the quoter wire, which is velocity's own assertion that the flow served
the swift hold or the book's activation delay. Velocity reads that co-signature once, at its own
boundary, so rotating a compromised flow key is one velocity admin call rather than a per-maker
migration.

The mid-staleness gate is the safety property: a dead price feed stops this quoter from quoting on
its own.
