# Instructions for agents

This repo is Velocity Protocol, a Solana perpetuals and spot exchange forked from Drift
protocol-v2. It holds the onchain programs, the TypeScript SDK and CLIs, the Rust SDK and keepers,
and the deployable services.

Every rule in this file applies to every task. Task-specific runbooks live in `docs/agents/`. Open
the one that matches your task before you start.

| Task                                                       | Read                                                                             |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------- |
| Build programs, toolchain errors, feature gates, SBPFv3    | [`docs/agents/build.md`](./docs/agents/build.md)                                 |
| Run or change tests, CI, formatting                        | [`docs/agents/testing.md`](./docs/agents/testing.md)                             |
| Any change under `programs/` or `packages/sdk/src/math/`   | [`docs/agents/type-discipline.md`](./docs/agents/type-discipline.md)             |
| Devnet upgrade, wipe, reinit                               | [`docs/agents/devnet.md`](./docs/agents/devnet.md)                               |
| Changesets, npm publish, Docker images, release CLI        | [`docs/agents/release.md`](./docs/agents/release.md)                             |
| Execution flows, account locations, SDK-to-instruction map | [`ARCHITECTURE.md`](./ARCHITECTURE.md)                                           |
| Adding fields to zero-copy structs                         | [`docs/alignment-and-native-offsets.md`](./docs/alignment-and-native-offsets.md) |
| Any prose you write into a file                            | [`.claude/skills/unslop/SKILL.md`](./.claude/skills/unslop/SKILL.md)             |

## Repo layout

The repo is a Bun workspace and a Turborepo monorepo. Use `bun`, not yarn or npm. Run
`bun install` once at the repo root and never inside a package. `packages/*` are the publishable
libraries (`@velocity-exchange/sdk`, `admin-cli`, `vaults-sdk`, `jit-proxy`). `apps/*` are private
services that ship as Docker images.

`rust/` is a separate Cargo workspace for velocity-rs, keep-rs and swift. The root `Cargo.toml`
excludes it, so its solana crate tree never unifies with the program build.

Programs live in `programs/`:

- `velocity/` is the core protocol. Instruction handlers are in `src/instructions/`
  (`user.rs`, `keeper.rs`, `admin.rs`, plus domain folders), state in `src/state/`, stateful
  operations in `src/controller/`, math in `src/math/`, and the VLP vAMM and hedge in `src/vlp/`.
- `vaults/` is the vaults program. Its client is `packages/vaults-sdk`. Regenerate its IDL and
  types with `bun run program:idl:vaults`.
- `protocol-revenue-router/` splits withdrawn protocol fees between the DFX recovery pool and the
  treasury. It is absent from `Anchor.toml [programs.localnet]`. Its tests are in the standalone
  workspace `programs/protocol-revenue-router/svm-tests/`. Its client is
  `packages/revenue-router-sdk`, regenerated with `bun run program:idl:revenue-router`.
- `jit-proxy/`, `pyth-lazer/`, `pyth/` and `token_faucet/` are described in `ARCHITECTURE.md`.

Switchboard and the external spot-fulfillment venues (Serum, Phoenix, OpenBook) were removed.
`OracleSource` keeps `DeprecatedSwitchboard` and `DeprecatedSwitchboardOnDemand` only to preserve
ABI discriminants, and both error out in `get_oracle_price`.

## Toolchain

On Apple Silicon, build with an x86_64 toolchain (`rustup default stable-x86_64-apple-darwin`),
never a native aarch64 one. Native ARM changes the zero-copy memory layout. Use Rust 1.89 or newer.

Build the programs with the `program:*` scripts in the root `package.json`, never with a bare
`anchor build`. The scripts carry the right feature flags for each program.

```bash
bun run program:build     # all test programs, then sync the IDL and types into packages/sdk/src/idl/
bun run program:idl       # IDL and types only, no SBF build
bun run build             # whole TypeScript workspace through turbo
cargo test -p velocity    # Rust unit tests
```

Any build error you don't recognize is probably in [`docs/agents/build.md`](./docs/agents/build.md).
Read it before debugging.

## Generated files

Never hand-edit these. Change the source and regenerate.

- `packages/sdk/src/idl/velocity.json` and `velocity.ts`. Regenerate with `bun run program:idl`.
  This is the only copy of the IDL in the repo. The SDK, `rust/velocity-rs/build.rs` and the
  `fuzz/e2e-svm*` harnesses all read it. Never add a second copy.
- `rust/velocity-rs/crates/src/velocity_idl.rs`. The next cargo build of `rust/` regenerates it
  from the IDL, and CI fails if the committed file is stale.
- The vaults and revenue-router IDLs and types, through their `program:idl:*` scripts.
- `package.json` versions. Changesets own them.

## Changes that need a second change

Each rule below names a change that silently breaks something else if you skip the companion edit.
Make both in the same PR.

**Struct, account or event changes in the IDL.** Update the matching type in
`packages/sdk/src/types.ts` (`UserAccount`, `PerpMarketAccount`, `StateAccount`, the `*Record`
events and the rest). These are hand-maintained mirrors. Nothing derives them from the IDL. That
covers a field being added, removed, renamed or reordered, a type change, and a width change
between `BN` and `number`. The IDL is the authority. Reconcile `types.ts` against it, never the
reverse.

**Program logic changes.** The SDK reimplements program logic in TypeScript: pricing, margin and
health, funding, fees, the AMM math in `packages/sdk/src/math/`, DLOB matching and auctions in
`packages/sdk/src/dlob/`, and the account abstractions in `user.ts` and `velocityClient.ts`. When
you change a formula, rounding, a threshold or clamp, validity gating, enum semantics, or the order
operations apply in, port the same change to the SDK. Port the same constants and edge cases. A
divergence produces no error. The SDK just mispredicts fills, margin, liquidation prices, funding
or fees. This applies even when the IDL is unchanged.

Verify the port against the program. A green SDK test proves nothing about parity, because SDK
tests pin the SDK's own previous output. In PRO-77, program commit `440349868` changed one AMM
spread formula, the SDK was never updated, and for three weeks every vAMM quote the SDK produced
was up to 16 times too narrow, while three spread tests in `packages/sdk/tests/` kept passing.

Shared fixtures in
[`packages/sdk/tests/sdkParity/fixtures/`](./packages/sdk/tests/sdkParity/fixtures/README.md) close
this gap for the spread math. The program's `parity_fixtures` tests and the SDK's
`ammSpread.test.ts` assert against the same CSV files, so a change to one side alone fails a test.
When you change a formula that has fixtures, regenerate the expected columns from the program and
review the diff. When you change a formula that has none, add fixtures in the same pattern. Read
[`docs/agents/type-discipline.md`](./docs/agents/type-discipline.md) for the rules.

When you change a formula in `programs/velocity/src/`, search `packages/sdk/src/math/` for its
mirror before opening the PR. When you update an SDK expectation because the program moved, say in
the commit message how you derived the new number.

**Admin instruction added, removed, renamed or re-signed.** Update `packages/cli-admin/`: the
wrapper in `src/commands/` (match the existing command style) and the command list in its
`README.md`. Verify with
`bunx turbo run build --filter=@velocity-exchange/admin-cli && bunx turbo run lint --filter=@velocity-exchange/admin-cli`.
The generic `call` dispatcher is an escape hatch, not a substitute for a wrapper.

**Anything a migrating integrator would notice.** Update
[`docs/DRIFT-TO-VELOCITY.md`](./docs/DRIFT-TO-VELOCITY.md). That covers instruction signatures and
accounts, account struct layouts, `Error` and `OracleSource` variants and other ABI-visible enums,
SDK exports, program IDs, PDA seeds and well-known addresses, dependency majors integrators
inherit (Anchor, web3.js), and protocol features added or removed. Follow the doc's structure:
features in sections 2 and 3, SDK in 4, ABI and layout in 5, and a row in the section 6 change
log. Update the section 7 checklist if the migration steps change. Verify every stated size, pubkey
and count against the code.

**User-facing change in a publishable package.** Add a changeset with `bun run changeset`. Chores,
CI and internal refactors don't need one. Details in
[`docs/agents/release.md`](./docs/agents/release.md).

**Gating job added or changed in `.github/workflows/main.yml`.** Mirror it in
`test-scripts/ci-local.sh`.

**Tag convention, workflow name or publish path changed in `.github/workflows/`.** Update
`deploy-scripts/release.sh`.

**Feature removed.** Search `deploy-scripts/` for helpers named after it and remove that phase, or
`init-devnet.ts` crashes at import on the next devnet run.

**Module behavior changed.** Update any module-level doc comment that the change makes wrong.

## Program rules

### Error enum stability

Onchain clients identify errors by numeric code, so the velocity `Error` enum is ABI-stable. Add
new variants at the bottom only. To remove one, mark it `/// @deprecated` and leave it in place.
Never delete or reorder variants.

### Oracle usage

Guard validity before you use an oracle price to drive any value transfer. A stale, divergent or
low-confidence price mis-sizes PnL, sweeps, settlements, liquidations and withdrawals.

- Mirror the gates the comparable existing path applies. `settle_pnl` runs
  `validate_market_within_price_band`, then for curve-update markets `is_recent_oracle_valid`, then
  `get_price_data_and_validity`, then `is_oracle_valid_for_action` or
  `is_price_divergence_ok_for_settle_pnl`, and requires the AMM to be fresh in the same slot through
  `is_fresh_at`. If you compute the same kind of value elsewhere, such as `net_user_pnl`, apply the
  same gates. Two paths that value the same thing and disagree is a bug.
- Pick the right price for the operation. TWAPs (`last_oracle_price_twap`, the 5 minute TWAP)
  resist manipulation and suit price-band and divergence checks. The live price suits immediate
  settlement once validity is confirmed. The confidence-bounded safe price is the third option.
- Use the shared helpers in `math/oracle.rs` and `state/oracle_map.rs`
  (`get_price_data_and_validity`, `is_oracle_valid_for_action`, `is_recent_oracle_valid`,
  `get_max_confidence_interval_multiplier`) instead of ad-hoc checks.
- Each `VelocityAction` variant carries its own validity tolerance. Pick the matching action or add
  one. Don't reuse an unrelated one.

### Zero-copy account structs

The size guards do not catch implicit padding. A `u64` or `u128` placed after trailing `u8` fields
makes the compiler insert alignment bytes. The SDK decodes sequentially from the IDL, reads the
field a few bytes early and returns garbage, while the trailing padding array keeps the total size
unchanged. Before you add or move a field, read
[`docs/alignment-and-native-offsets.md`](./docs/alignment-and-native-offsets.md) for the layout
invariants and [`docs/agents/type-discipline.md`](./docs/agents/type-discipline.md) for the
required assertions.

### Feature gates

`isolated-position` and `vlp-hedge` compile their instructions out of mainnet builds pending
audit. State and interior logic stay in every build. A gated instruction must never share a
`#[derive(Accounts)]` struct with an ungated one, or the `cpi` build breaks in a way
`cargo check -p velocity` doesn't catch. Run `cargo check -p vaults` too. Details in
[`docs/agents/build.md`](./docs/agents/build.md).

### Instruction layout

For a new instruction domain, or one being substantially reworked, follow
`instructions/protocol_fees/`. Each domain is a folder with one file per instruction and a `mod.rs`
holding the domain doc comment and re-exports. In each file the `#[derive(Accounts)]` struct goes at
the top and the handler below it. An instruction that belongs in an existing monolithic file
(`user.rs`, `keeper.rs`, `admin.rs`) follows that file's structure. Don't split a file just to add
one instruction.

Put account identity checks in the accounts struct: PDA `seeds` and `bump` (including seeds derived
from another account's loaded field), `has_one` for top-level pubkey fields, and `address =` locks.
Keep a check in the handler only when it is multi-account or stateful logic, math on loaded data,
or a data invariant rather than an identity check.

### Constants

Policy values (thresholds, defaults, caps) and precision constants go in
`programs/velocity/src/math/constants.rs`, not next to the function that uses them. Keep a local
`const` only for values with no meaning outside that function.

### Other patterns

`ARCHITECTURE.md` covers the native high-frequency entrypoint (opcodes behind
`[0xFF, 0xFF, 0xFF, 0xFF, opcode]`), the `remaining_accounts` conventions, `AccountLoader` for large
accounts, and the feature flags.

## Before you hand off

CI fails the PR unless these are clean. Run them before you call the work done.

- Rust: `bun run fmt:rust` (nightly rustfmt, see [`docs/agents/testing.md`](./docs/agents/testing.md))
  and the two clippy commands in [`docs/agents/testing.md`](./docs/agents/testing.md#formatting-and-lint).
  Any clippy warning fails CI, in tests and in both build flavors.
- SDK: `cd packages/sdk/ && bun run prettify && bun run lint`.
- When you touch a feature-gated subsystem, check both flavors as described in `build.md`.

## Writing

Every piece of prose you write into the repo follows the unslop rules in
[`.claude/skills/unslop/SKILL.md`](./.claude/skills/unslop/SKILL.md). Open that file before you
write. It does not load on its own. Claude Code imports it through `CLAUDE.md`, and other agents
have to read it.

The rules apply to docs, READMEs, code comments, doc comments, commit messages, PR descriptions,
changesets and `DRIFT-TO-VELOCITY.md` rows. Apply them to the lines you add or rewrite. Leave
existing text alone unless the task is to rewrite it, and never rewrite another person's text
without being asked.

Repo conventions on top of unslop:

- Write "onchain" and "offchain", not "on-chain".
- Code comments are optional. Add one only when the reason behind the code isn't obvious. Keep it to
  one line where possible, and write a full sentence, not a fragment. State the rule the code
  follows, not the history of how it got there.
- Never put spec or ticket references ("Tier A", "F-1", "WS5", issue numbers) in code comments.
  Those go in the PR description or commit message.
- Module `//!` doc comments are a title line and one prose paragraph, as in `vlp/amm/mod.rs`. Don't
  list submodules one by one.

## Git

Never add Claude or any other AI assistant as `Co-Authored-By` on commits, and never add a
"Generated with" footer to commits or PR descriptions. Write commit messages and PR descriptions as
the human author.
