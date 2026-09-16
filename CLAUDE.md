# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

For execution flow maps, the module responsibility matrix, account type locations, and the mapping
between SDK calls and program instructions, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Package manager

Use `bun` for JavaScript and TypeScript dependencies, not yarn or npm: `bun install`,
`bun run <script>`.

This repo is a Bun workspace and a Turborepo monorepo. The root `package.json` declares
`workspaces: ["packages/*", "apps/*"]`, governed by a single root `bun.lock`. Run `bun install`
**once at the repo root**; do not install inside individual packages. `packages/*` are the
publishable libraries (`@velocity-exchange/sdk`, `@velocity-exchange/admin-cli`,
`@velocity-exchange/vaults-sdk`). `apps/*` are the deployable services, which are private and ship
as Docker images.

TypeScript builds run through Turbo. `bun run build` (which is `turbo run build`) builds the whole
graph in dependency order. `bunx turbo run build --filter=<pkg>` builds one package and its
dependencies.

A second, separate Cargo workspace lives at `rust/` and holds velocity-rs, keep-rs and swift. See
the "Rust SDK and keeper workspace" section below. It is excluded from the program workspace by
`exclude = ["rust"]` in the root `Cargo.toml`.

## Build

**On Apple Silicon, always use an x86_64 cross-compile toolchain, never a native aarch64 one.**
Native ARM toolchains break the memory layout expectations of zero-copy accounts, which must match
the onchain x86_64 representation.

- Anchor 0.29.x branches: `rustup default 1.76.0-x86_64-apple-darwin`
- Anchor 1.0 branches: `rustup default stable-x86_64-apple-darwin`

**Rust version and zero-copy struct alignment.** Rust 1.77 corrected `align_of::<u128>()` to 16
bytes on x86_64. The onchain SBF target has always kept it at 8. Every zero-copy struct in this
repo is explicitly padded so that `(SIZE - 8) % 16 == 0`, and u128 and i128 fields are ordered
before any `PoolBalance` fields, which makes `sizeof` identical on all targets regardless of Rust
version.

Develop and test with Rust 1.77 or newer anyway. That way x86_64 exercises real 16-byte u128
alignment, and a future struct change that breaks the invariant fires the `const_assert_eq!` guards
locally instead of diverging onchain. Anchor 1.0 branches require Rust 1.89 or newer, which is the
Anchor 1.0 MSRV. [`docs/alignment-and-native-offsets.md`](./docs/alignment-and-native-offsets.md)
has the full invariant rules and covers adding fields to zero-copy structs.

**For the Solana programs, use the `program:*` scripts in the root package.json.** They encode the
correct feature flags so you do not have to remember them.

```bash
bun run program:build           # program + IDL/types synced into packages/sdk/src/idl/ (devnet/test flavor)
bun run program:idl             # IDL/types only, no SBF build. Fast path for layout/name changes
bun run program:build:devnet    # deployable devnet .so (wraps deploy-scripts/build-devnet.sh)
bun run program:build:mainnet   # mainnet .so (default features: production gates on, devnet ixs compiled out)
```

`program:build` and `program:idl` use `--no-default-features --features no-entrypoint,anchor-test`.
That is required even though `declare_id!` is now unconditional, because default features include
`mainnet-beta`. `mainnet-beta` compiles out the devnet-only instructions, including
`force_wipe_accounts_devnet`, which `wipe-devnet.ts` calls through the SDK IDL, and it switches
`ids.rs` to the mainnet constants. `program:idl` runs `anchor idl build` under `cargo test` with the
host toolchain, which avoids the bundled-cargo problems described below.

### Post-audit feature gates (`isolated-position`, `vlp-hedge`)

The instructions for isolated perp positions and for the VLP hedge and LP-pool component are
compiled out of mainnet builds, meaning the default feature set, pending audit. `anchor-test`
implies both features, so `program:build`, `program:idl` and all tests keep them. `build-devnet.sh`
enables them explicitly, so devnet keeps them live.

Only the instructions are gated. All state stays compiled into every build, including
`PerpPosition.isolated_position_scaled_balance`, `PerpMarket.hedge_config` and the LP-pool
accounts, along with the interior logic, so account layouts never diverge between flavors.

**A gated instruction must not share a `#[derive(Accounts)]` struct with an ungated one.** Anchor's
`cpi` module deduplicates the per-struct re-export and inherits the cfg, so the ungated
instruction's cpi accounts type disappears. The build then breaks only when the `cpi` feature is on,
and a plain `cargo check -p velocity` does not catch it. Run `cargo check -p vaults` as well, or
`cargo check -p velocity --features cpi`.

This is why five lp-pool admin config instructions stay ungated:
`update_perp_market_lp_pool_id`, `update_perp_market_lp_pool_paused_operations`, and the three
`update_feature_bit_flags_*_lp_pool` instructions. They are inert config writes whose readers are
compiled out.

To enable either subsystem on mainnet, add the features to the mainnet build invocation and upgrade
in place. When you touch either subsystem, check that both flavors compile:
`cargo check -p velocity` for the gated build, and
`cargo check -p velocity --no-default-features --features no-entrypoint,anchor-test` for the
enabled one.

**SDK:**

```bash
bun install                                          # once, at the repo root (workspace)
bunx turbo run build --filter=@velocity-exchange/sdk # build the SDK (+ its deps)
# or `bun run build` to build the whole TS workspace
```

**Update IDL after program changes:**

`packages/sdk/src/idl/velocity.json` and `packages/sdk/src/idl/velocity.ts` are generated
artifacts. Never hand-edit them. To change them, modify the Rust program and regenerate with
`bun run program:build`, or `bun run program:idl` for the fast path. A manual edit drifts from the
onchain layout with no error and breaks clients. A full `anchor build` already emits both
`target/idl/velocity.json` and `target/types/velocity.ts`, and the scripts only copy them into
`packages/sdk/src/idl/`, so no separate `anchor idl build` or `anchor idl type` step is needed after
a full build.

`packages/sdk/src/idl/velocity.json` is the single copy of the IDL in the repo. The TypeScript SDK,
`rust/velocity-rs`'s `build.rs`, and the `fuzz/e2e-svm*` harnesses all read that one file. Never add
a second copy. A duplicate turns every IDL change into a multi-file diff and can go stale.

**Keep `packages/sdk/src/types.ts` in sync with the IDL.** The TypeScript types there
(`UserAccount`, `PerpMarketAccount`, `SpotMarketAccount`, `StateAccount`, `AMM`, the `*Record` event
types and the rest) are hand-maintained mirrors of the onchain structs. Nothing derives them from
the IDL: the SDK does not use Anchor's `IdlAccounts`, `IdlTypes` or `IdlEvents` helpers, because
those cannot generate the enum variant classes or the SDK-only types.

So whenever a struct, account or event changes in the IDL, update the corresponding type in
`types.ts` in the same change. That covers a field being added, removed, renamed or reordered, a
type change, and a width change between `BN` and `number`. The file header already states this
contract. Treat the IDL as the authoritative layout source and reconcile `types.ts` against it,
never the reverse.

**Mirror Rust program logic changes in the TypeScript SDK.** Beyond layout, the SDK re-implements
chunks of the program's logic in TypeScript: pricing, margin and health, funding, fees, the AMM math
in `packages/sdk/src/math/`, the DLOB matching and auction logic in `packages/sdk/src/dlob/`, and
the account abstractions in `user.ts` and `velocityClient.ts`.

So whenever you change program behavior, update the corresponding TypeScript in the same change, so
the SDK stays a faithful offchain mirror. That covers a formula, rounding or precision, a threshold
or clamp, validity gating, an enum's semantics, the order in which operations apply, and any other
computation a client must reproduce to predict onchain results.

A divergence between the Rust computation and its TypeScript counterpart is a bug, and it produces
no error. The SDK just mispredicts fills, margin, liquidation prices, funding or fees. Port the same
constants and the same edge-case handling rather than approximating, and update or extend the SDK
unit tests that pin the behavior (`cd packages/sdk/ && bun run test:ci`). This applies even when the
IDL and layout are unchanged. A pure logic change still requires a matching SDK update.

**Update the admin CLI when admin instructions change:**

`packages/cli-admin/` wraps the admin/keeper surface. Whenever admin instructions are added, removed, renamed, or change signature, update the CLI in the same change: add/remove the dedicated wrapper in `packages/cli-admin/src/commands/` (mirroring the existing command style), update `packages/cli-admin/README.md`'s command list, and verify with `bunx turbo run build --filter=@velocity-exchange/admin-cli && bunx turbo run lint --filter=@velocity-exchange/admin-cli` (CI builds the whole TS workspace on every PR via the `ts-build` job). The generic `call` dispatcher is an escape hatch, not a substitute for wrappers on routinely-used operations.

### macOS build environment

Two pitfalls that fresh setups regularly hit. If you see either symptom, apply the matching fix before debugging anything else.

**Symptom: `c/blake3_impl.h:4:10: fatal error: 'assert.h' file not found`** during `anchor build`.
The Solana platform-tools clang has no built-in macOS SDK path; it can't find system headers. Fix:

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
```

(or prefix the build command with it). Add it to your shell profile so future sessions inherit it.

**Symptom: `feature 'edition2024' is required ... not stabilized in this version of Cargo (1.84.0)`** when downloading `toml_datetime` / `wincode` / `toml_parser`.
The bundled cargo in older platform-tools (v1.51 ships cargo 1.84) cannot parse `edition2024` deps. Fix it by upgrading platform-tools. The Anchor 1.0 branches need v1.54 or newer:

```bash
cargo-build-sbf --tools-version v1.54 --force-tools-install
```

Run that once; subsequent `anchor build` invocations will use the new toolchain. Check with `cargo-build-sbf --version`.

**Symptom: `could not execute process .../1.89.0-sbpf-solana-v1.52/bin/rustc (never executed)`** during an SBF build.
The platform-tools payload is present under `~/.cache/solana/<version>/` but its rustup toolchain link is missing, and `cargo-build-sbf` picks its own default version rather than whichever one you last installed, so having v1.54 linked does not help when it wants v1.52. Link the version it is asking for:

```bash
rustup toolchain link 1.89.0-sbpf-solana-v1.52 ~/.cache/solana/v1.52/platform-tools/rust
```

Substitute the version from the error path. This is machine-level state, not repo state, so it recurs on any fresh worktree or new machine until linked.

**Symptom: program panics with `Access violation in unknown section at address 0x80 of size 8`** (or similar address) at runtime, on instructions that touch types you didn't change.
This is almost always stale SBF build artifacts after a Cargo.lock dep change. SBF caches compiled `.rlib`s under `target/sbpf-solana-solana/`, and the cache key does not catch every dep-resolution change, so the resulting `.so` loads but reads and writes wrong offsets. Whenever Cargo.lock dep versions change (e.g. after `cargo update`, or after switching branches with different lockfiles), do:

```bash
rm -rf target/sbpf-solana-solana target/deploy
cargo-build-sbf --tools-version v1.54 -- --features anchor-test
```

## Testing

**Local CI emulation:** `bash test-scripts/ci-local.sh` runs the gating checks from
`.github/workflows/main.yml` locally (`--fast` = static checks only, `--full` adds the
anchor/vault integration suites and rust-workspace tests). **Keep `test-scripts/ci-local.sh`
in sync with the CI workflow**: whenever a gating job in `.github/workflows/main.yml` is
added, removed, or its command changes, mirror the change in `ci-local.sh` in the same PR.
The script also encodes two local-only traps CI never hits:
- the SBF cache-poisoning guard (see the access-violation runbook entry above): it wipes
`target/sbpf-solana-solana` before the integration-suite build, since `.so` files built on a
cache that mixed feature flavors die at entry with `Access violation in unknown section`;
- the IDL-flavor restore: the anchor suite's own build (default features = `mainnet-beta` ON)
syncs an IDL with devnet-only instructions compiled out into `packages/sdk/src/idl/`
(committing that breaks `wipe-devnet.ts`), so after the suites the script reruns
`bun run program:idl` to restore the canonical flavor.

**Build each program with its own feature flags, not velocity's.** `run-vault-tests.sh` builds
velocity, vaults and the fixtures separately on purpose. A single
`anchor build -- --no-default-features --features no-entrypoint,anchor-test` applies velocity's
flags to every program: `--no-default-features` strips the vaults, pyth and token_faucet
entrypoints, which produces 896-byte stub `.so` files. Bankrun then reports
`Program is not deployed` and every vault `before` hook fails with
`invalid account data for instruction 2`. Vaults separately needs `--features anchor-test` for the
test `admin::ID` in `constants.rs` that the fee-update tests sign with; without it `is_admin`
rejects with `0x7d3`.

**Cargo unifies features across workspace members.** `cargo test --workspace --features X` turns X
on for every member, not just the one you meant. The offline Rust gate stays clean only because no
member enables `rpc_tests`. Live tests are split out under that feature rather than hidden behind
`#[ignore]`.

**The `apps/*` jest suites use swc, not ts-jest.** They share
`jest.config.app.cjs` (@swc/jest plus `jest.setup.app.ts`). The app sources were authored against
swc semantics: they need `jest.mock` factory hoisting and must not type-check at run time, because
app sources are not strictly type-clean against the SDK types. ts-jest fails both ways.
`isolatedModules: true` skips mock hoisting, and `isolatedModules: false` blocks on the type
errors. Each wired package has a one-line `jest.config.cjs` re-exporting the preset. The `ts-tests`
CI job enumerates the app suites explicitly rather than globbing `./apps/*`, so an unwired app
cannot slip into the gate unnoticed.

**Rust unit tests:**

```bash
cargo test -p velocity                    # velocity program only
cargo test -p velocity -- --show-output  # with stdout
```

**Single TypeScript integration test:**

```bash
ts-mocha -t 300000 ./tests/<test_file>.ts
```

**Full TypeScript integration test suite** (builds first, then runs all ~70 test files serially):

```bash
bash test-scripts/run-anchor-tests.sh
# Skip rebuild if .so is already built:
bash test-scripts/run-anchor-tests.sh --skip-build
```

The integration tests in `tests/` import the SDK by relative path (`../packages/sdk/src/...`) and resolve `@coral-xyz/anchor` and friends from the repo-root `node_modules`. The single root `bun install` provides both. There is no separate per-package install. If deps are missing, run `bun install` at the repo root.

**SDK unit tests:**

```bash
cd packages/sdk/ && bun run test:dlob    # DLOB tests
cd packages/sdk/ && bun run test:ci      # CI subset
```

**Lint/format:**

```bash
bun run fmt:rust                 # all Rust in the repo (wraps nightly rustfmt, see below)
bun run fmt:rust:check           # verify without writing (what CI enforces)
cd packages/sdk/ && bun run prettify:fix  # SDK (TypeScript)
```

**Rust formatting requires nightly rustfmt.** The repo's `rustfmt.toml` sets
`imports_granularity = "One"` / `group_imports = "One"` (all `use` items in a module merged into a
single `use { ... }` block, solana-labs style). Those are nightly-only options. Stable `cargo fmt`
warns and ignores them, so newly added imports stay unmerged and CI's nightly fmt check fails.
Install once with `rustup toolchain install nightly --component rustfmt` and always format via
`bun run fmt:rust` (wraps `scripts/fmt-rust.sh`, which covers every Rust codebase in the repo).
Only formatting uses nightly; builds, clippy, and tests stay on the stable toolchains above. CI pins the exact nightly in `RUST_NIGHTLY_TOOLCHAIN` (`.github/workflows/main.yml`).
The generated `rust/velocity-rs/crates/src/velocity_idl.rs` and `rust/keep-rs/vendor/` are on the
rustfmt ignore list, because codegen pipes through stable rustfmt and vendored code keeps upstream
formatting. Keep them stable-formatted.

The style is enforced on every Rust codebase in the repo, including the standalone workspaces the
two `cargo +nightly fmt` invocations above don't reach: each `fuzz/<crate>/` is fmt-checked by its
`fuzz-build` CI matrix job (format one locally with
`cargo +nightly fmt --manifest-path fuzz/<crate>/Cargo.toml --all`), and the non-member
`rust/velocity-rs/examples/*` crates (which don't resolve under cargo) are checked with raw
`rustup run <nightly> rustfmt --edition <crate edition>` in the rust-workspace CI job.

**Always run `bun run fmt:rust` and `cargo clippy -p velocity` before declaring Rust work complete.** CI runs the equivalent of `bun run fmt:rust:check` (spread across jobs) and `cargo clippy -p velocity` (see `.github/workflows/main.yml`) and will fail the PR otherwise. The equivalent SDK gate is `cd packages/sdk/ && bun run prettify` + `bun run lint`. Do not hand off a change until those commands are clean.

## Rust SDK + keeper workspace (`rust/`)

`rust/` is a second Cargo workspace holding the imported Rust crates: `velocity-rs` (Rust SDK),
`keep-rs` (keeper bots, binary `keeprs`), and `swift` (tx server, binary `swift-server`). It is
deliberately separate from the program workspace (root `Cargo.toml` has `exclude = ["rust"]`) so its
split solana 4.2 crate tree never unifies with the program's SBF build. It has its own
`rust/Cargo.lock` and builds into `rust/target/` (via `rust/.cargo/config.toml`), never clobbering
`./target`. Build/check it with `cargo check --manifest-path rust/Cargo.toml` (or `bun run rust:build`).

- These crates consume the velocity program as a host library path-dep: `drift = { package = "velocity", path = "../../programs/velocity", ... }`. That host build is independent of `cargo build-sbf`.
- **IDL tie:** `velocity-rs/build.rs` regenerates `velocity-rs/crates/src/velocity_idl.rs` from the canonical program IDL `packages/sdk/src/idl/velocity.json`, the same file the TypeScript SDK consumes, read directly across the workspace with no vendored copy. `bun run program:idl` regenerates that IDL; the next `cargo build`/`cargo check` of the rust workspace picks it up and recompiles the types. `velocity_idl.rs` is committed and the build script is external-consumer-safe: when the IDL json is absent (velocity-rs consumed as a git dep outside a full checkout, vendored, or packaged) it falls back to the committed file instead of erroring, and it only rewrites the file when the generated content actually differs (so read-only / checksum-verified source dirs like `cargo vendor` and Nix build cleanly). The `rust-workspace-check` CI job fails if the committed `velocity_idl.rs` is out of sync with the IDL. Never hand-edit `velocity_idl.rs`. It is generated. (Pre-monorepo, velocity-rs vendored a fetched copy at `res/velocity.json` kept in sync by a `rust:idl-sync` script; both are removed.)
- `keep-rs`'s `[patch.crates-io]` and `swift`'s `[profile.dev.package]` are hoisted into `rust/Cargo.toml` (Cargo only honors patches/profiles at the workspace root). `keep-rs/vendor/pyth-lazer-protocol` is un-ignored in `.gitignore`.
- The velocity fork removed some upstream-drift features (IF-rebalance / `ProtocolIfSharesTransferConfig`, gov-token staking). When importing newer velocity-rs/keep-rs/swift, expect to drop references to removed types (see the import commits for the pattern).

## Apps and Docker images

`apps/*` are the deployable services, all `private` and never published to npm: `dlob-server`,
`keeper-bots-v2` and `usermap-server`. The infrastructure-v3 services (`candles`, `market-data`,
`multisig-monitor`, `notification-engine`, `realtime-archiver`, `aggregator-api`) and their
`@backend/*` support libs are not part of this monorepo. They deploy from `infrastructure-v3`.

Pushing a git tag `docker-<app>-v<version>` triggers `.github/workflows/velocity-publish.yml`,
which builds and pushes that app's image to ECR (eu-west-1, via OIDC). The map from app to build metadata is
`docker-info.json` (path, turbo scope, output dir/entrypoint or cargo bin, ECR repo). TS apps
build via `docker/ts-app.Dockerfile` (full-context bun + turbo); Rust apps (`keep-rs`, `swift`) via
`docker/rust-app.Dockerfile`. The version is everything after the last `-v`, so app keys may
contain `-v` (e.g. `docker-keeper-bots-v2-v1.4.2`). Add a new app by adding a `docker-info.json` entry.

## Publishing (changesets)

Library packages under `packages/*` publish via [changesets](https://github.com/changesets/changesets),
NOT release-please (removed). Add a changeset in your PR (`bun run changeset`); merging the
auto-maintained "Version Packages" PR commits the version bumps; then push a tag `npm-<pkg>-v<version>`
to trigger `.github/workflows/npm-publish.yml`, which builds and publishes that one package via npm
OIDC trusted publishing. `<pkg>` is the directory name under `packages/`:

| Package                         | Tag example             |
| ------------------------------- | ----------------------- |
| `@velocity-exchange/sdk`        | `npm-sdk-v0.2.3`        |
| `@velocity-exchange/admin-cli`  | `npm-cli-admin-v0.2.3`  |
| `@velocity-exchange/vaults-sdk` | `npm-vaults-sdk-v0.2.3` |
| `@velocity-exchange/jit-proxy`  | `npm-jit-proxy-v0.2.3`  |

The tag version must match the `package.json` version set by the "Version Packages" PR. The workflow
is idempotent and skips the publish if that version is already on the registry. `npm` (not `bun`) is used
for publishing because bun does not implement npm's OIDC trusted-publishing flow; workspace dep ranges
are rewritten to concrete versions by `.github/scripts/rewrite-workspace-deps.mjs` before publish.

**Release CLI:** `bun run release <status|bump|devnet|npm|docker|infra|mainnet>` (`deploy-scripts/release.sh`) drives the tag pushes, workflow dispatches and infra pin updates above, in release order; it is read-only until `--execute` is passed. See the "Release CLI" section of [`deploy-scripts/README.md`](./deploy-scripts/README.md). When a tag convention, workflow name or publish path changes in `.github/workflows/`, update the script in the same change.

**PRs that change user-facing behavior in a publishable package should include a changeset.** That covers new features, bug fixes and API changes, but not chores, CI config, or internal refactors that do not affect consumers. To add one, run `bun run changeset` at the repo root, select the affected package(s), choose the bump type (patch/minor/major), and write a short description. Commit the generated `.changeset/*.md` file with your changes. Do not manually edit `package.json` versions. Changesets and the "Version Packages" bot own those fields.

## Devnet program upgrade

Full runbook lives in [`deploy-scripts/README.md`](./deploy-scripts/README.md). Read its "Operational notes" section before any devnet upgrade. The key rules:

- **Always use a private RPC** for `solana program` and `anchor program upgrade` writes. velocity.so is around 5 MB, which is roughly 5,000 chunked writes, and `api.devnet.solana.com` rate-limits the upload partway through every time. Velocity's Triton URL is recorded in memory `reference_velocity_devnet_rpc.md`. Also `solana config set --url <url>` so the underlying CLI inherits it.
- **Prefer the two-phase deploy over `anchor program upgrade`.** Drive `deploy-scripts/write-buffer-devnet.sh` (creates / resumes a named on-chain buffer) and then `deploy-scripts/deploy-from-buffer-devnet.sh` (one-tx swap). `anchor program upgrade` creates an anonymous buffer and auto-closes it on failure, so the next retry restarts from chunk 0; the two-phase flow keeps the buffer pubkey on disk so re-running `write-buffer-devnet.sh` resumes by only re-sending chunks that didn't land.
- **Resume until done.** `write-buffer` can exit 0 with the buffer still partial. Verify with `solana program show <BUFFER_PK>`; Data Length must be at least the .so size. If `deploy-from-buffer` fails with `Failed to parse ELF file: invalid section header` or `invalid account data for instruction`, the buffer is partial. Re-run `write-buffer-devnet.sh` against the same buffer keypair and try again.
- **Reclaim rent from orphaned buffers** (~38 SOL each for velocity-sized buffers): `solana program show --buffers [--buffer-authority <pk>]` to list, `solana program close --buffers --recipient <pk> --buffer-authority <keypair>` to close all under one authority. Check both the CLI default keypair and the upgrade-authority keypair as candidate authorities.
- **Anchor 1.0 renamed `anchor upgrade` to `anchor program upgrade`.** `deploy-devnet.sh` uses the new form.

After a successful upgrade with a layout-breaking change, run `deploy-scripts/wipe-devnet.ts` (calls the devnet-only `force_wipe_accounts_devnet` ix) then `deploy-scripts/init-devnet.sh` to recreate state under the new layouts.

### Wipe-and-reinit pitfalls

Read this before touching the wipe path. Each item below cost someone real time to work out.

- **SPL token vaults survive a velocity-only wipe.** Solana rule: only the owning program can decrement an account's lamports. `force_wipe_accounts_devnet` zeroes velocity-owned PDAs but cannot touch `spot_market_vault` / `insurance_fund_vault` (Token-program owned). After a wipe these vaults linger and `initialize_spot_market` then fails with `Allocate: account ... already in use` because Anchor's `init` constraint unconditionally calls System Allocate on the same PDA address.
- **Closing an SPL token account requires `amount == 0`.** Token program rejects `close_account` with `Non-native account can only be closed if its balance is zero` (error `0xb`). The wipe ix must `spl_token::burn` or transfer before closing, and `burn` needs the mint passed as a writable account. The wipe-devnet.ts script reads each vault's data on chain to find its mint and passes `(vault, mint)` pairs in `remaining_accounts`.
- **Mixing manual lamport mutation with CPI in one loop trips the runtime.** Solana's per-CPI conservation check fires with `sum of account balances before and after instruction do not match` if you manually credit admin lamports and then CPI into another program that also rebalances lamports. Fix: do all CPI closes in one pass, then all manual drains in a second pass.
- **The IDL regen recipe must drop `mainnet-beta` or devnet-only ixs vanish from the IDL.** Default features include `mainnet-beta`, which strips `#[cfg(not(feature = "mainnet-beta"))]` items. The deployed `.so` _has_ the ix (built via `build-devnet.sh` with `--no-default-features`) but `program.methods.forceWipeAccountsDevnet` is undefined on the SDK because the IDL doesn't list it. Use: `anchor idl build -p velocity -o target/idl/velocity.json -- --no-default-features --features no-entrypoint,anchor-test` then `cp` and `anchor idl type`.
- **Anchor 1.0 `.accounts()` is implicitly `accountsPartial` and may reorder.** When sending a wipe ix with explicit `velocitySigner` + `tokenProgram`, the auto-resolver can shift them into `remaining_accounts`. Use `.accountsStrict({...})` for fixed account sets.
- **`deploy-from-buffer-devnet.sh` insists on a `PROGRAM_KEYPAIR` file.** An upgrade does not need the program keypair, only the upgrade authority. Run it directly: `solana program deploy target/deploy/velocity.so --buffer <BUF_PK> --program-id <PROGRAM_PUBKEY> --upgrade-authority <KP> -u <URL>` (`--program-id` accepts a Pubkey for upgrades).
- **`wipe-devnet.ts` walks `.wiped-*.json` archives too**, not just the active receipt. Any spot-market index ever recorded gets its derived vault PDAs included in subsequent wipes. Don't delete the archives until you're certain there are no lingering on-chain accounts.
- **Phase G (LP pool) creates more orphan token accounts.** The LP-pool subaccounts (e.g. dUSDT constituent token vault) survive a wipe the same way as spot vaults. If you don't need an LP pool, `SKIP_PHASE_G=1`. Otherwise extend the `wipe-devnet.ts` collector to derive the LP-pool vault PDAs.
- **Removing a feature breaks the deploy scripts.** PR #38's PMM removal is the example: the SDK
  exports that `init-devnet.ts` imports disappeared, so the script crashed at import. After any
  feature removal, search `deploy-scripts/` for helpers named after that feature and remove the
  phase before the next devnet run.

## Git / commit conventions

**Never add Claude (or any AI assistant) as a `Co-Authored-By` on commits, PR bodies, or anywhere else in version control.** Write commit messages and PR descriptions as the human author. No `🤖 Generated with …` footers either.

## Architecture

This is Velocity Protocol v1, a Solana perpetuals and spot trading protocol.

### Programs (`programs/`)

- **`velocity/`**: Core protocol (Anchor, ~500k+ lines of Rust). Entry point: `src/lib.rs`. Main instruction handlers in `src/instructions/`:
  - `user.rs`: trading instructions (place/cancel/fill orders)
  - `keeper.rs`: keeper/crank instructions (settle PnL, funding, liquidations)
  - `admin.rs`: admin/governance instructions
  - `lp_pool.rs`, `lp_admin.rs`: LP pool management
- **`vaults/`**: Velocity vaults program (Anchor 1.0; program id `vAuLTsyrv…`). Depends on the `velocity` program as a host/CPI path-dep, referenced by its real crate name `velocity` (not the `program` alias velocity-rs uses, because anchor's IDL build resolves dependency programs by name, so `velocity` maps to `programs/velocity`). Its TS client is `packages/vaults-sdk` (`@velocity-exchange/vaults-sdk`). Regenerate the SDK's IDL + types from the program with `bun run program:idl:vaults` (writes `packages/vaults-sdk/src/idl/vaults.json` and `src/types/vaults.ts`). Never hand-edit them.
- **`pyth-lazer/`**: Pyth Lazer message/payload/signature/storage types, linked into `velocity` as a real library dependency and used by `instructions/pyth_lazer_oracle.rs` (not a CPI target).
- **`pyth/`**: Pyth V1 account layout types, an optional dependency of `velocity` pulled in only by the `fuzz-fixtures` feature (plus a dev-dependency for tests).
- **`jit-proxy/`**: Just-in-time fill/arb proxy program; CPIs into `velocity` (depends on it with the `cpi` feature).
- **`token_faucet/`**: Devnet/test token minting utility.

Switchboard oracle support and external spot-fulfillment venues (Serum, Phoenix, OpenBook) were removed from the protocol; there is no `programs/switchboard*` or `programs/openbook_v2`. The `OracleSource` enum keeps `DeprecatedSwitchboard`/`DeprecatedSwitchboardOnDemand` variants only to preserve ABI discriminants. Both error out in `get_oracle_price`.

### SDK (`packages/sdk/`)

TypeScript library (`@velocity-exchange/sdk`). Key modules in `src/`:

- `velocityClient.ts`: main client class
- `user.ts`: user account abstraction
- `dlob/`: Decentralized Limit Order Book implementation
- `math/`: pricing, margin, funding math
- `idl/velocity.json`: generated Anchor IDL (do not edit manually)

### Tests (`tests/`)

~70 TypeScript integration tests using ts-mocha + Anchor's local validator (bankrun for some). Each test spins up a local validator with the program deployed. Tests are run serially by `run-anchor-tests.sh`.

### Program internals

- `programs/velocity/src/math/`: core math (funding, fees, margin, AMM)
- `programs/velocity/src/state/`: account structs (User, PerpMarket, SpotMarket, etc.)
- `programs/velocity/src/controller/`: stateful operations (position updates, fills, liquidations)
- `programs/velocity/src/validation/`: pre-instruction validation

### Instruction module layout

Preferred layout for instruction code (reference: `instructions/protocol_fees/`): each instruction domain is a folder under `src/instructions/` with one file per instruction and a `mod.rs` that holds the domain-level doc comment and re-exports. Within each instruction file, the `#[derive(Accounts)]` context struct goes at the top, the handler below it. Use this pattern for new instruction domains and when an existing domain is being substantially reworked anyway. However, if an instruction belongs under one of the existing monolithic instruction trees (`user.rs`, `keeper.rs`, `admin.rs`, …), follow that file's established structure instead. Do not split a tree just to add one instruction.

**Prefer constraints over in-handler validation when the check is trivial.** Account _identity_ checks belong on the accounts struct, not in the handler: PDA `seeds`/`bump` derivation (including deriving one account's seeds from another's loaded field, e.g. `seeds = [b"spot_market", perp_market.load()?.quote_spot_market_index.to_le_bytes().as_ref()]`), `has_one` for top-level pubkey fields (e.g. `has_one = oracle`), and `address =` locks. Only keep a check in the handler when it is genuinely non-trivial as a constraint: multi-account/stateful logic, math on loaded data, or a _data invariant_ rather than an account identity. Don't contort complex logic into constraint expressions just to move it.

### Doc comments

All modules have doc comments. When making feature or refactor changes, update any module-level doc comments that would be invalidated by the change.

### Migration doc ([docs/DRIFT-TO-VELOCITY.md](./docs/DRIFT-TO-VELOCITY.md))

`docs/DRIFT-TO-VELOCITY.md` is the canonical record of what changed between upstream `velocity-exchange/protocol-v2` (fork point `0ae3e3b1d`) and Velocity, written for external integrators migrating off the old Drift SDK/program. **Whenever a change could affect a migrating integrator, update this doc in the same PR.** That includes:

- Adding, removing, or changing the signature/accounts of a program instruction
- Changing the layout, size, or fields of any on-chain account struct (User, PerpMarket, SpotMarket, State, …)
- Adding or deprecating `Error` enum variants, `OracleSource` variants, or other ABI-visible enums
- Renaming, removing, or adding SDK exports (classes, types, constants, config fields)
- Changing program IDs, PDA seeds, mints, or other well-known addresses
- Changing dependency majors that integrators inherit (Anchor, @solana/web3.js)
- Removing or adding a protocol feature

Match the doc's existing structure: feature-level changes go in §2/§3, SDK surface in §4, ABI/layout notes in §5, and add a row to the PR change log in §6. Update the §7 checklist if the migration steps themselves change. Keep stated facts (sizes, pubkeys, counts) verified against the code, not guessed. Purely internal refactors that don't change the program ABI or SDK surface do not need a doc update.

### Error enum stability

The velocity program's `Error` enum is ABI-stable, because onchain clients identify errors by numeric code. When modifying it:

- **Add** new variants at the bottom only, never insert between existing ones.
- **Remove** by marking the variant as deprecated, for example `/// @deprecated`, and leaving it in place. Do not delete or reorder.

### Oracle usage

**Any time you read an oracle price to drive a value transfer, guard its validity first. Never trust a raw oracle price.** A stale, divergent or low-confidence oracle can mis-size any amount derived from it, including PnL, sweeps, settlements, liquidations and withdrawals. When adding or reviewing code that touches an oracle:

- **Gate on validity before using the price.** Mirror the checks the comparable existing path already applies. `settle_pnl`, for example, runs `validate_market_within_price_band`, then for curve-update markets calls `is_recent_oracle_valid`, then `get_price_data_and_validity`, then `is_oracle_valid_for_action` or `is_price_divergence_ok_for_settle_pnl`, and it requires the AMM to be fresh in the same slot via `is_fresh_at`. If you compute the same kind of value elsewhere, such as `net_user_pnl`, apply the same gates. Two code paths that value the same thing and disagree is a bug.
- **Consider which price you should actually be using.** The spot/last oracle price is not always correct. Choose between the live price, the confidence-bounded safe price, and the TWAP (`last_oracle_price_twap`, the 5 minute twap and so on) for the operation at hand. TWAPs resist manipulation, which suits price-band and divergence checks. Live prices suit immediate settlement once validity is confirmed.
- **Prefer the shared helpers** in `math/oracle.rs` and `state/oracle_map.rs` (`get_price_data_and_validity`, `is_oracle_valid_for_action`, per-market `is_recent_oracle_valid` / `get_max_confidence_interval_multiplier`) over ad-hoc checks, so behavior stays consistent across instructions.
- New `VelocityAction` variants exist so each action can express its own validity tolerance. Pick the matching action, or add one, rather than reusing an unrelated one.

### Key design patterns

- Velocity has a custom native entrypoint alongside the standard Anchor `#[program]` one, for a few
  high-frequency keeper instructions that bypass Anchor's overhead. It matches the discriminator
  `[0xFF, 0xFF, 0xFF, 0xFF, opcode]` and wires exactly three opcodes (`lib.rs`): 0 is
  `update_mm_oracle_native`, 1 is `update_amm_spread_adjustment_native`, 2 is
  `update_mm_oracle_batch_native`. Everything else falls through to the Anchor entrypoint.
- `remaining_accounts` is used extensively to pass variable numbers of oracle accounts, spot markets, and maker accounts to instructions.
- Zero-copy account loading (`AccountLoader`) is used for large accounts (User, PerpMarket).
- Feature flags: `mainnet-beta` (production gates), `anchor-test` (enables test helpers), `no-entrypoint`/`cpi` (for SDK dependencies).
