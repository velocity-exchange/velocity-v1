# clob-wire-v2

`crates/clob-wire`'s source, compiled a second time against the anchor v2 fork for the CLOB
program's own IDL. `src/lib.rs` is one line, `include!("../../clob-wire/src/lib.rs")`: there is no
second copy of the instruction types to keep in sync. Edit `crates/clob-wire`; this crate picks up
the change on its next build.

## Dependents

- `anchor-v2/programs/clob`, the only program that reaches these types through the fork.

## Why a second crate

The CLOB builds against an alpha branch of anchor's next major version, forked at
`https://github.com/otter-sec/anchor.git`, while velocity builds against the official `anchor-lang`
1.0.2. The two are separate crates with the same name and incompatible IDL derive traits, so a
single crate cannot derive its wire types against both at once. This crate `include!`s
`clob-wire`'s definitions and derives them against the fork instead, depending on
`quoter-spec-v2` rather than `quoter-spec` so every layer of the include chain sees the same
anchor. `crates/quoter-spec-v2`'s README covers the mechanism in more detail; it applies here
unchanged.

## Build and test

Builds as part of the `anchor-v2` workspace:

```
bun run program:build:clob
```

It has no tests of its own; `crates/clob-wire`'s tests cover the shared source, and
`anchor-v2/programs/clob`'s own suite (`bun run test:clob`) exercises it as a dependency.
