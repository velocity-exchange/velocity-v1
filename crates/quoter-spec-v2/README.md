# quoter-spec-v2

`crates/quoter-spec`'s source, compiled a second time against the anchor v2 fork so the CLOB and
the midpoint programs can build their IDL against it. `src/lib.rs` is one line,
`include!("../../quoter-spec/src/lib.rs")`: there is no second copy of the wire types to keep in
sync. Edit `crates/quoter-spec`; this crate picks up the change on its next build.

## Dependents

- `anchor-v2/programs/clob`
- `anchor-v2/programs/midpoint`
- `crates/clob-wire-v2` and `crates/clob-state-v2`, which reuse the same `UserRefV0` and `SideV0`
  this crate exports.

## Why a second crate

Velocity depends on the official `anchor-lang` 1.0.2 from crates.io. The CLOB and the midpoint
depend on an alpha branch of anchor's next major version, forked at
`https://github.com/otter-sec/anchor.git`. The two trees are different crates with the same name
and incompatible `IdlBuild`/`IdlType` traits, so a single crate cannot derive against both: whichever
`anchor-lang` Cargo resolves is the only one every derive on that crate sees. `quoter-spec` derives
against the real one for velocity. This crate `include!`s the same struct and enum definitions and
derives against the fork instead, gated behind an `extern crate anchor_lang_v2 as anchor_lang`
alias the base file switches on with `#[cfg(feature = "idl-build-v2")]`. One anchor per crate is
what lets the fork's IDL build see one dependency graph and velocity's IDL build see the other,
with no divergence between the two struct layouts because there is only one place they are
written.

## Build and test

Builds as part of the `anchor-v2` workspace:

```
bun run program:build:clob
bun run program:build:midpoint
```

It has no tests of its own; `crates/quoter-spec`'s `tests.rs` covers the shared source, and
`anchor-v2/programs/clob`'s and `midpoint`'s own test suites exercise it as a dependency.
