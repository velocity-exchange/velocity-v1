# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

For execution flow maps, module responsibility matrix, account type locations, and SDK↔program mappings, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Package Manager

Use `bun` (not yarn/npm) for JavaScript/TypeScript dependency management: `bun install`, `bun run <script>`.

This repo is a **Bun workspace + Turborepo monorepo**. The root `package.json` declares
`workspaces: ["packages/*", "apps/*"]`, governed by a single root `bun.lock` — run `bun install`
**once at the repo root** (do NOT install inside individual packages). `packages/*` are the
publishable libraries (`@velocity-exchange/sdk`, `@velocity-exchange/admin-cli`,
`@velocity-exchange/vaults-sdk`); `apps/*` are the deployable services (private, shipped as Docker
images). TypeScript
builds run through Turbo: `bun run build` (= `turbo run build`) builds the whole graph in dependency
order; `bunx turbo run build --filter=<pkg>` builds one package + its deps.

There is also a **second, separate Cargo workspace** at `rust/` (velocity-rs, keep-rs, swift) — see the
"Rust SDK + keeper workspace" section below. It is excluded from the program workspace
(`exclude = ["rust"]` in the root `Cargo.toml`).

## Build

**M1/Apple Silicon:** Always use an x86_64 cross-compile toolchain — never a native aarch64 toolchain. Native ARM toolchains break memory layout expectations for zero-copy accounts, which must match the on-chain (x86_64) representation.

- Anchor 0.29.x branches: `rustup default 1.76.0-x86_64-apple-darwin`
- Anchor 1.0 branches: `rustup default stable-x86_64-apple-darwin`

**Rust version and zero-copy struct alignment:** Rust ≥ 1.77 corrected `align_of::<u128>()` to 16 bytes on x86_64; the on-chain SBF target has always kept it at 8 bytes. All zero-copy structs in this repo are explicitly padded so `(SIZE - 8) % 16 == 0` and u128/i128 fields are ordered before any `PoolBalance` fields — this makes `sizeof` identical on all targets regardless of Rust version. You must still develop and test with Rust ≥ 1.77 so that x86_64 exercises real 16-byte u128 alignment and any future struct change that breaks the invariant is caught locally (the `const_assert_eq!` guards fire) rather than silently diverging on-chain. The minimum for Anchor 1.0 branches is Rust ≥ 1.89 (Anchor 1.0 MSRV). See [`docs/alignment-and-native-offsets.md`](./docs/alignment-and-native-offsets.md) for the full invariant rules and guidance on adding fields to zero-copy structs.

**Solana programs (Rust/Anchor):** use the `program:*` scripts in the root package.json — they encode the correct feature flags so you don't have to remember them.

```bash
bun run program:build           # program + IDL/types synced into packages/sdk/src/idl/ (devnet/test flavor)
bun run program:idl             # IDL/types only, no SBF build — fast path for layout/name changes
bun run program:build:devnet    # deployable devnet .so (wraps deploy-scripts/build-devnet.sh)
bun run program:build:mainnet   # mainnet .so (default features: production gates on, devnet ixs compiled out)
```

`program:build` and `program:idl` use `--no-default-features --features no-entrypoint,anchor-test`. This is required even though `declare_id!` is now unconditional: default features include `mainnet-beta`, which compiles out devnet-only instructions (e.g. `force_wipe_accounts_devnet` — `wipe-devnet.ts` calls it via the SDK IDL) and switches `ids.rs` to mainnet constants. `program:idl` runs `anchor idl build` under `cargo test` with the host toolchain, which sidesteps the bundled-cargo issues described below.

**Post-audit feature gates (`isolated-position`, `vlp-hedge`):** the instruction surface of isolated perp positions and the VLP hedge/LP-pool component is compiled out of mainnet builds (default features) pending audit. `anchor-test` implies both features, so `program:build`, `program:idl`, and all tests keep them; `build-devnet.sh` enables them explicitly, so devnet keeps them live. Only instructions are gated — all state (`PerpPosition.isolated_position_scaled_balance`, `PerpMarket.hedge_config`, LP-pool accounts) and interior logic stay compiled in every build so account layouts never diverge. **A gated instruction must not share a `#[derive(Accounts)]` struct with an ungated one**: anchor's `cpi` module dedups the per-struct re-export and inherits the cfg, so the ungated instruction's cpi accounts type vanishes and the build breaks only when the `cpi` feature is on — a plain `cargo check -p velocity` does not catch it; also run `cargo check -p vaults` (or `cargo check -p velocity --features cpi`). This is why five lp-pool admin config ixs (`update_perp_market_lp_pool_id`, `..._paused_operations`, three `update_feature_bit_flags_*_lp_pool`) remain ungated — they are inert config writes whose readers are compiled out. To enable on mainnet: add the features to the mainnet build invocation and upgrade in place. When touching either subsystem, verify both flavors compile: `cargo check -p velocity` (gated) and `cargo check -p velocity --no-default-features --features no-entrypoint,anchor-test` (enabled).

**SDK:**

```bash
bun install                                          # once, at the repo root (workspace)
bunx turbo run build --filter=@velocity-exchange/sdk # build the SDK (+ its deps)
# or `bun run build` to build the whole TS workspace
```

**Update IDL after program changes:**

NEVER hand-edit `packages/sdk/src/idl/velocity.json` or `packages/sdk/src/idl/velocity.ts` — they are generated artifacts. To change them, modify the Rust program and regenerate (`bun run program:build`, or `bun run program:idl` for the fast path). Manual edits will silently drift from on-chain layout and break clients. Note a full `anchor build` already emits both `target/idl/velocity.json` and `target/types/velocity.ts`; the scripts just copy them into `packages/sdk/src/idl/` — no separate `anchor idl build`/`anchor idl type` step is needed after a full build.

`packages/sdk/src/idl/velocity.json` is the single copy of the IDL in the repo. The TypeScript SDK, `rust/velocity-rs`'s `build.rs`, and the `fuzz/e2e-svm*` harnesses all read that one file. Never add a second copy — a duplicate turns every IDL change into a multi-file diff and can go stale.

**Keep `packages/sdk/src/types.ts` in sync with the IDL.** The TypeScript types in `packages/sdk/src/types.ts` (`UserAccount`, `PerpMarketAccount`, `SpotMarketAccount`, `StateAccount`, `AMM`, the `*Record` event types, etc.) are **hand-maintained mirrors** of the on-chain structs — they are NOT derived from the IDL automatically (the SDK does not use Anchor's `IdlAccounts`/`IdlTypes`/`IdlEvents` helpers, because the enum variant classes and SDK-only types can't be generated). Whenever a struct, account, or event changes in the IDL (a field is added, removed, renamed, reordered, or its type changes — including `BN` ↔ `number` width differences), update the corresponding type in `types.ts` in the same change so the mirror stays faithful to the regenerated IDL. The file header already states this contract; treat the IDL as the authoritative layout source and reconcile `types.ts` against it, never the reverse.

**Mirror Rust program logic changes in the TypeScript SDK.** Beyond layout, the SDK re-implements
chunks of the program's *logic* in TypeScript — pricing, margin/health, funding, fees, AMM math
(`packages/sdk/src/math/`), the DLOB matching/auction logic (`packages/sdk/src/dlob/`), and account
abstractions (`user.ts`, `velocityClient.ts`). Whenever you change program behavior — a formula,
rounding/precision, a threshold or clamp, validity gating, an enum's semantics, the order in which
operations apply, or any other computation a client must reproduce to predict on-chain results —
update the corresponding TypeScript so the SDK stays a faithful off-chain mirror, **in the same
change**. A silent divergence between the Rust computation and its TS counterpart is a bug: it makes
the SDK mispredict fills, margin, liquidation prices, funding, or fees. Port the same constants and
edge-case handling (don't approximate), and update/extend the SDK unit tests
(`cd packages/sdk/ && bun run test:ci`) that pin the behavior. This applies even when the IDL/layout is
unchanged — pure logic changes still require a matching SDK update.

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
The bundled cargo in older platform-tools (v1.51 ships cargo 1.84) can't parse `edition2024` deps. Fix by upgrading platform-tools — the Anchor 1.0 branches need ≥ v1.54:

```bash
cargo-build-sbf --tools-version v1.54 --force-tools-install
```

Run that once; subsequent `anchor build` invocations will use the new toolchain. Check with `cargo-build-sbf --version`.

**Symptom: `could not execute process .../1.89.0-sbpf-solana-v1.52/bin/rustc (never executed)`** during an SBF build.
The platform-tools payload is present under `~/.cache/solana/<version>/` but its rustup toolchain link is missing, and `cargo-build-sbf` picks its own default version rather than whichever one you last installed — so having v1.54 linked does not help when it wants v1.52. Link the version it is asking for:

```bash
rustup toolchain link 1.89.0-sbpf-solana-v1.52 ~/.cache/solana/v1.52/platform-tools/rust
```

Substitute the version from the error path. This is machine-level state, not repo state, so it recurs on any fresh worktree or new machine until linked.

**Symptom: program panics with `Access violation in unknown section at address 0x80 of size 8`** (or similar address) at runtime, on instructions that touch types you didn't change.
This is almost always **stale SBF build artifacts** after a Cargo.lock dep change. SBF caches compiled `.rlib`s under `target/sbpf-solana-solana/`, and the cache key doesn't catch every dep-resolution change — the resulting `.so` loads but reads/writes wrong offsets. Whenever Cargo.lock dep versions change (e.g. after `cargo update`, or after switching branches with different lockfiles), do:

```bash
rm -rf target/sbpf-solana-solana target/deploy
cargo-build-sbf --tools-version v1.54 -- --features anchor-test
```

**Symptom: `Access violation in stack frame 3 at address 0x2000...` on instructions that were fine before** (first seen: `initialize_user_stats` on a local validator), and a clean rebuild does NOT fix it.
This is a **platform-tools v1.52 miscompile**, not a stale cache: v1.52 (the default bundled with `cargo-build-sbf` 3.1.14, which plain `anchor build` uses) emits velocity code that overflows a 4KB stack frame at runtime; v1.54 compiles the same code correctly. Any velocity `.so` that will actually be *executed* (validator deploys, the e2e localnet harness, devnet buffers) must be built with `--tools-version v1.54` explicitly — do not rely on the default:

```bash
cargo-build-sbf --tools-version v1.54 --manifest-path programs/velocity/Cargo.toml -- --no-default-features --features no-entrypoint,anchor-test
```

The litesvm/bankrun suites can mask this: they exercise only the instructions each test calls, and older runtimes were lenient. `integration-tests/tests/init_probe.rs` pins the real `initialize_user_stats` path so a miscompiled `.so` fails fast.

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

The integration tests in `tests/` import the SDK by **relative path** (`../packages/sdk/src/...`) and resolve `@coral-xyz/anchor` and friends from the **repo-root** `node_modules`. The single root `bun install` (workspace) provides both — there is no separate per-package install. If deps are missing, run `bun install` at the repo root.

**Rust integration tests (litesvm, `integration-tests/`):** a standalone workspace that loads the
real `.so` fixtures and drives real instructions. It needs three built programs first — velocity, and
the CLOB + midpoint from `anchor-v2/`:

```bash
cargo-build-sbf --tools-version v1.54 --manifest-path programs/velocity/Cargo.toml -- --no-default-features --features no-entrypoint,anchor-test
bun run program:build:clob && bun run program:build:midpoint
cd integration-tests && cargo test --locked
```

Gated in CI by the `integration-tests` job in `.github/workflows/main.yml`.

**SDK unit tests:**

```bash
cd packages/sdk/ && bun run test:dlob    # DLOB tests
cd packages/sdk/ && bun run test:ci      # CI subset
```

### Which suites to run while working

The long suites are minutes-to-tens-of-minutes each and rebuild the SBF program. **Do not re-run them
repeatedly inside a working session** — the loop is: unit tests while iterating, **one** integration
run before you push, CI for everything else.

| While iterating (seconds–a minute, run freely)                                                            | Once before pushing                                            | CI only                                                 |
| --------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- | ------------------------------------------------------- |
| `cargo test -p velocity`, `cargo check -p velocity`, `cargo check -p vaults`, `cargo clippy -p velocity`   | `bash test-scripts/run-anchor-tests.sh` (`--skip-build` if the `.so` is current) | `vault-tests`, `rust-workspace-check`, `docker-images-*` |
| `cd packages/sdk && bun run test:ci`, `bun run test:dlob`                                                  | `cd integration-tests && cargo test --locked`                   | the fuzz workflow                                       |

When an integration suite fails, **read the failure and fix the cause** — do not re-run it hoping for a
different result, and do not re-run the whole file set to check one test. Re-run the single failing
test (`ts-mocha -t 300000 ./tests/<file>.ts`, or `cargo test --locked <test_name>` in
`integration-tests/`), then do the one full run at the end.

**`bun run test:e2e:localnet` is a manual, local-only gate — never part of an iteration loop, and
deliberately not in CI.** It stands up a real `solana-test-validator`, a `redis-server`, the Rust
book-publisher and swift-server, and a relay crank-turner, and it needs a **separate `relay`
checkout** (`RELAY_REPO`, default `~/source/relay`) whose program and turner it builds from source.
That last dependency is why it is not CI-feasible today: relay is a different private repo, so a CI
job would need a deploy key plus a pinned relay revision, and the run would be a ~40-minute
multi-service job whose failures are usually the harness rather than the change. Run it locally
before a devnet upgrade and after touching the relay-facing surface (condition blocks, resolvers,
executors). If it ever needs to gate, the prerequisite is pinning relay as a submodule (or vendoring
`relay.so` + the turner binary) — recommend that before wiring the job, don't approximate it with a
partial harness.

**Lint/format:**

```bash
bun run fmt:rust                 # all Rust in the repo (wraps nightly rustfmt — see below)
bun run fmt:rust:check           # verify without writing (what CI enforces)
cd packages/sdk/ && bun run prettify:fix  # SDK (TypeScript)
```

**Rust formatting requires nightly rustfmt.** The repo's `rustfmt.toml` sets
`imports_granularity = "One"` / `group_imports = "One"` (all `use` items in a module merged into a
single `use { ... }` block, solana-labs style), which are nightly-only options — stable `cargo fmt`
warns and ignores them, so newly added imports stay unmerged and CI's nightly fmt check fails.
Install once with `rustup toolchain install nightly --component rustfmt` and always format via
`bun run fmt:rust` (wraps `scripts/fmt-rust.sh`, which covers every Rust codebase in the repo).
Only formatting uses nightly; builds, clippy, and tests stay on the stable toolchains above. CI pins the exact nightly in `RUST_NIGHTLY_TOOLCHAIN` (`.github/workflows/main.yml`).
The generated `rust/velocity-rs/crates/src/velocity_idl.rs` and `rust/keep-rs/vendor/` are on the
rustfmt ignore list (codegen pipes through stable rustfmt; vendored code keeps upstream formatting) —
keep them stable-formatted.

The style is enforced on every Rust codebase in the repo, including the standalone workspaces the
two `cargo +nightly fmt` invocations above don't reach: each `fuzz/<crate>/` is fmt-checked by its
`fuzz-build` CI matrix job (format one locally with
`cargo +nightly fmt --manifest-path fuzz/<crate>/Cargo.toml --all`), and the non-member
`rust/velocity-rs/examples/*` crates (which don't resolve under cargo) are checked with raw
`rustup run <nightly> rustfmt --edition <crate edition>` in the rust-workspace CI job.

**Always run `bun run fmt:rust` and `cargo clippy -p velocity` before declaring Rust work complete.** CI runs the equivalent of `bun run fmt:rust:check` (spread across jobs) and `cargo clippy -p velocity` (see `.github/workflows/main.yml`) and will fail the PR otherwise. The equivalent SDK gate is `cd packages/sdk/ && bun run prettify` + `bun run lint`. Do not hand off a change until those commands are clean.

## Rust SDK + keeper workspace (`rust/`)

`rust/` is a **second Cargo workspace** holding the imported Rust crates: `velocity-rs` (Rust SDK),
`keep-rs` (keeper bots, binary `keeprs`), and `swift` (tx server, binary `swift-server`). It is
deliberately separate from the program workspace (root `Cargo.toml` has `exclude = ["rust"]`) so its
solana-sdk 3.x dependency tree never unifies with the program's SBF build. It has its own
`rust/Cargo.lock` and builds into `rust/target/` (via `rust/.cargo/config.toml`), never clobbering
`./target`. Build/check it with `cargo check --manifest-path rust/Cargo.toml` (or `bun run rust:build`).

- These crates consume the velocity program as a **host library** path-dep: `drift = { package = "velocity", path = "../../programs/velocity", ... }`. That host build is independent of `cargo build-sbf`.
- **IDL tie:** `velocity-rs/build.rs` regenerates `velocity-rs/crates/src/velocity_idl.rs` from the **canonical** program IDL `packages/sdk/src/idl/velocity.json` — the same file the TypeScript SDK consumes, read directly across the workspace (no vendored copy). `bun run program:idl` regenerates that IDL; the next `cargo build`/`cargo check` of the rust workspace picks it up and recompiles the types. `velocity_idl.rs` is **committed** and the build script is external-consumer-safe: when the IDL json is absent (velocity-rs consumed as a git dep outside a full checkout, vendored, or packaged) it falls back to the committed file instead of erroring, and it only rewrites the file when the generated content actually differs (so read-only / checksum-verified source dirs like `cargo vendor` and Nix build cleanly). The `rust-workspace-check` CI job fails if the committed `velocity_idl.rs` is out of sync with the IDL. Never hand-edit `velocity_idl.rs` — it is generated. (Pre-monorepo, velocity-rs vendored a fetched copy at `res/velocity.json` kept in sync by a `rust:idl-sync` script; both are removed.)
- `keep-rs`'s `[patch.crates-io]` and `swift`'s `[profile.dev.package]` are **hoisted** into `rust/Cargo.toml` (Cargo only honors patches/profiles at the workspace root). `keep-rs/vendor/pyth-lazer-protocol` is un-ignored in `.gitignore`.
- The velocity fork **removed** some upstream-drift features (IF-rebalance / `ProtocolIfSharesTransferConfig`, gov-token staking). When importing newer velocity-rs/keep-rs/swift, expect to drop references to removed types (see the import commits for the pattern).

## Apps and Docker images

`apps/*` are the deployable services (all `private`, never published to npm): `dlob-server` and
`keeper-bots-v2`. The infrastructure-v3 services (`candles`, `market-data`, `multisig-monitor`,
`notification-engine`, `realtime-archiver`, `aggregator-api`) and their `@backend/*` support libs
are **not** part of this monorepo — they deploy from `infrastructure-v3`.

Pushing a git tag **`docker-<app>-v<version>`** triggers `.github/workflows/docker-on-tag.yml`,
which builds and pushes that app's image to ECR (eu-west-1, via OIDC). The app→metadata map is
**`docker-info.json`** (path, turbo scope, output dir/entrypoint or cargo bin, ECR repo). TS apps
build via `docker/ts-app.Dockerfile` (full-context bun + turbo); Rust apps (`keep-rs`, `swift`) via
`docker/rust-app.Dockerfile`. The version is everything after the **last** `-v`, so app keys may
contain `-v` (e.g. `docker-keeper-bots-v2-v1.4.2`). Add a new app by adding a `docker-info.json` entry.

## Publishing (changesets)

Library packages under `packages/*` publish via [changesets](https://github.com/changesets/changesets),
NOT release-please (removed). Add a changeset in your PR (`bun run changeset`); merging the
auto-maintained "Version Packages" PR commits the version bumps; then push a tag **`npm-<pkg>-v<version>`**
to trigger `.github/workflows/npm-publish.yml`, which builds and publishes that one package via npm
OIDC trusted publishing. `<pkg>` is the directory name under `packages/`:

| Package                         | Tag example             |
| ------------------------------- | ----------------------- |
| `@velocity-exchange/sdk`        | `npm-sdk-v0.2.3`        |
| `@velocity-exchange/admin-cli`  | `npm-cli-admin-v0.2.3`  |
| `@velocity-exchange/vaults-sdk` | `npm-vaults-sdk-v0.2.3` |

The tag version must match the `package.json` version set by the "Version Packages" PR. The workflow
is idempotent — it skips publish if that version is already on the registry. `npm` (not `bun`) is used
for publishing because bun does not implement npm's OIDC trusted-publishing flow; workspace dep ranges
are rewritten to concrete versions by `.github/scripts/rewrite-workspace-deps.mjs` before publish.

**PRs that change user-facing behavior in a publishable package should include a changeset.** This includes new features, bug fixes, and API changes — but not chores, CI config, or internal refactors that don't affect consumers. To add one: run `bun run changeset` at the repo root, select the affected package(s), choose the bump type (patch/minor/major), and write a short description. Commit the generated `.changeset/*.md` file with your changes. Do not manually edit `package.json` versions — changesets and the "Version Packages" bot own those fields.

**One changeset per feature branch.** While a branch is unmerged, it carries exactly one `.changeset/*.md` file; every later change on the branch folds into that file in place. The changeset becomes the published release notes, and a consumer only ever sees the branch's final surface — so rewrite it to describe that final surface, and delete anything an intra-branch change superseded ("X was renamed to Y" is noise when X never shipped). Never add a second changeset for the same branch.

## Devnet program upgrade

Full runbook lives in [`deploy-scripts/README.md`](./deploy-scripts/README.md). Read its "Operational notes" section before any devnet upgrade. The key rules:

- **Always use a private RPC** for `solana program` / `anchor program upgrade` writes — velocity.so is ~5 MB (~5,000 chunked writes) and `api.devnet.solana.com` reliably rate-limits the upload partway through. Velocity's Triton URL is recorded in memory `reference_velocity_devnet_rpc.md`. Also `solana config set --url <url>` so the underlying CLI inherits it.
- **Prefer the two-phase deploy over `anchor program upgrade`.** Drive `deploy-scripts/write-buffer-devnet.sh` (creates / resumes a named on-chain buffer) and then `deploy-scripts/deploy-from-buffer-devnet.sh` (one-tx swap). `anchor program upgrade` creates an anonymous buffer and auto-closes it on failure, so the next retry restarts from chunk 0; the two-phase flow keeps the buffer pubkey on disk so re-running `write-buffer-devnet.sh` resumes by only re-sending chunks that didn't land.
- **Resume until done.** `write-buffer` can exit 0 with the buffer still partial. Verify with `solana program show <BUFFER_PK>` — Data Length must be ≥ the .so size. If `deploy-from-buffer` fails with `Failed to parse ELF file: invalid section header` / `invalid account data for instruction`, the buffer is partial — re-run `write-buffer-devnet.sh` against the same buffer keypair and re-attempt.
- **Reclaim rent from orphaned buffers** (~38 SOL each for velocity-sized buffers): `solana program show --buffers [--buffer-authority <pk>]` to list, `solana program close --buffers --recipient <pk> --buffer-authority <keypair>` to close all under one authority. Check both the CLI default keypair and the upgrade-authority keypair as candidate authorities.
- **Anchor 1.0 renamed `anchor upgrade` → `anchor program upgrade`.** `deploy-devnet.sh` uses the new form.

After a successful upgrade with a layout-breaking change, run `deploy-scripts/wipe-devnet.ts` (calls the devnet-only `force_wipe_accounts_devnet` ix) then `deploy-scripts/init-devnet.sh` to recreate state under the new layouts.

### Wipe-and-reinit pitfalls

Each item below cost real time before being understood — read this before touching the wipe path.

- **SPL token vaults survive a velocity-only wipe.** Solana rule: only the owning program can decrement an account's lamports. `force_wipe_accounts_devnet` zeroes velocity-owned PDAs but cannot touch `spot_market_vault` / `insurance_fund_vault` (Token-program owned). After a wipe these vaults linger and `initialize_spot_market` then fails with `Allocate: account ... already in use` because Anchor's `init` constraint unconditionally calls System Allocate on the same PDA address.
- **Closing an SPL token account requires `amount == 0`.** Token program rejects `close_account` with `Non-native account can only be closed if its balance is zero` (error `0xb`). The wipe ix must `spl_token::burn` (or transfer) before closing — and `burn` needs the mint passed as a writable account. The wipe-devnet.ts script reads each vault's data on chain to find its mint and passes `(vault, mint)` pairs in `remaining_accounts`.
- **Mixing manual lamport mutation with CPI in one loop trips the runtime.** Solana's per-CPI conservation check fires with `sum of account balances before and after instruction do not match` if you manually credit admin lamports and then CPI into another program that also rebalances lamports. Fix: do all CPI closes in one pass, then all manual drains in a second pass.
- **The IDL regen recipe must drop `mainnet-beta` or devnet-only ixs vanish from the IDL.** Default features include `mainnet-beta`, which strips `#[cfg(not(feature = "mainnet-beta"))]` items. The deployed `.so` _has_ the ix (built via `build-devnet.sh` with `--no-default-features`) but `program.methods.forceWipeAccountsDevnet` is undefined on the SDK because the IDL doesn't list it. Use: `anchor idl build -p velocity -o target/idl/velocity.json -- --no-default-features --features no-entrypoint,anchor-test` then `cp` and `anchor idl type`.
- **Anchor 1.0 `.accounts()` is implicitly `accountsPartial` and may reorder.** When sending a wipe ix with explicit `velocitySigner` + `tokenProgram`, the auto-resolver can shift them into `remaining_accounts`. Use `.accountsStrict({...})` for fixed account sets.
- **`deploy-from-buffer-devnet.sh` insists on a `PROGRAM_KEYPAIR` file.** For an _upgrade_ you don't need the program keypair — only the upgrade authority. Direct: `solana program deploy target/deploy/velocity.so --buffer <BUF_PK> --program-id <PROGRAM_PUBKEY> --upgrade-authority <KP> -u <URL>` (`--program-id` accepts a Pubkey for upgrades).
- **`wipe-devnet.ts` walks `.wiped-*.json` archives too**, not just the active receipt. Any spot-market index ever recorded gets its derived vault PDAs included in subsequent wipes. Don't delete the archives until you're certain there are no lingering on-chain accounts.
- **Phase G (LP pool) creates more orphan token accounts.** The LP-pool subaccounts (e.g. dUSDT constituent token vault) survive a wipe the same way as spot vaults. If you don't need an LP pool, `SKIP_PHASE_G=1`. Otherwise extend the `wipe-devnet.ts` collector to derive the LP-pool vault PDAs.
- **Removing a feature (e.g. PR #38 PMM removal) breaks deploy scripts.** SDK exports referenced by `init-devnet.ts` / `verify-devnet.ts` vanish; the script crashes at import. After any feature removal, search `deploy-scripts/` for helpers named after it and rip that phase out before the next devnet run.

## Git / commit conventions

**Never add Claude (or any AI assistant) as a `Co-Authored-By` on commits, PR bodies, or anywhere else in version control.** Write commit messages and PR descriptions as the human author. No `🤖 Generated with …` footers either.

## Architecture

This is **Velocity Protocol v1** — a Solana perpetuals and spot trading protocol.

### Programs (`programs/`)

- **`velocity/`** — Core protocol (Anchor, ~500k+ lines of Rust). Entry point: `src/lib.rs`. Main instruction handlers in `src/instructions/`:
  - `user.rs` — trading instructions (place/cancel/fill orders)
  - `keeper.rs` — keeper/crank instructions (settle PnL, funding, liquidations)
  - `admin.rs` — admin/governance instructions
  - `lp_pool.rs`, `lp_admin.rs` — LP pool management
- **`vaults/`** — Velocity vaults program (Anchor 1.0; program id `vAuLTsyrv…`). Depends on the `velocity` program as a host/CPI path-dep, referenced by its real crate name `velocity` (not the `program` alias velocity-rs uses — anchor's IDL build resolves dependency programs by name, so `velocity` maps to `programs/velocity`). Its TS client is `packages/vaults-sdk` (`@velocity-exchange/vaults-sdk`). Regenerate the SDK's IDL + types from the program with `bun run program:idl:vaults` (writes `packages/vaults-sdk/src/idl/vaults.json` + `src/types/vaults.ts`) — never hand-edit them.
- **`pyth-lazer/`** — Pyth Lazer message/payload/signature/storage types, linked into `velocity` as a real library dependency and used by `instructions/pyth_lazer_oracle.rs` (not a CPI target).
- **`pyth/`** — Pyth V1 account layout types, an optional dependency of `velocity` pulled in only by the `fuzz-fixtures` feature (plus a dev-dependency for tests).
- **`jit-proxy/`** — Just-in-time fill/arb proxy program; CPIs into `velocity` (depends on it with the `cpi` feature).
- **`token_faucet/`** — Devnet/test token minting utility.

Switchboard oracle support and external spot-fulfillment venues (Serum, Phoenix, OpenBook) were removed from the protocol; there is no `programs/switchboard*` or `programs/openbook_v2`. The `OracleSource` enum keeps `DeprecatedSwitchboard`/`DeprecatedSwitchboardOnDemand` variants only to preserve ABI discriminants — both error out in `get_oracle_price`.

### SDK (`packages/sdk/`)

TypeScript library (`@velocity-exchange/sdk`). Key modules in `src/`:

- `velocityClient.ts` — main client class
- `user.ts` — user account abstraction
- `dlob/` — Decentralized Limit Order Book implementation
- `math/` — pricing, margin, funding math
- `idl/velocity.json` — generated Anchor IDL (do not edit manually)

### Tests (`tests/`)

~70 TypeScript integration tests using ts-mocha + Anchor's local validator (bankrun for some). Each test spins up a local validator with the program deployed. Tests are run serially by `run-anchor-tests.sh`.

### Program internals

- `programs/velocity/src/math/` — core math (funding, fees, margin, AMM)
- `programs/velocity/src/state/` — account structs (User, PerpMarket, SpotMarket, etc.)
- `programs/velocity/src/controller/` — stateful operations (position updates, fills, liquidations)
- `programs/velocity/src/validation/` — pre-instruction validation

### Instruction module layout

Preferred layout for instruction code (reference: `instructions/protocol_fees/`): each instruction domain is a **folder** under `src/instructions/` with **one file per instruction** and a `mod.rs` that holds the domain-level doc comment and re-exports. Within each instruction file, the `#[derive(Accounts)]` context struct goes at the **top**, the handler below it. Use this pattern for new instruction domains and when an existing domain is being substantially reworked anyway. However, if an instruction belongs under one of the existing monolithic instruction trees (`user.rs`, `keeper.rs`, `admin.rs`, …), follow that file's established structure instead — don't split a tree just to add one instruction.

**Constraints over in-handler validates — when trivial.** Account _identity_ checks belong on the accounts struct, not in the handler: PDA `seeds`/`bump` derivation (including deriving one account's seeds from another's loaded field, e.g. `seeds = [b"spot_market", perp_market.load()?.quote_spot_market_index.to_le_bytes().as_ref()]`), `has_one` for top-level pubkey fields (e.g. `has_one = oracle`), and `address =` locks. Only keep a check in the handler when it is genuinely non-trivial as a constraint: multi-account/stateful logic, math on loaded data, or a _data invariant_ rather than an account identity. Don't contort complex logic into constraint expressions just to move it.

### Function and module shape

The reference for this is `controller/orders/`: one 7,967-line file became a 343-line module root
over nine subject modules, and its fill path went from four functions of 571, 196, 155 and 331
lines to a chain whose largest step is 70.

**Size limits.** A function body stays around 60 lines or under, and takes six arguments or fewer.
These are not arbitrary: a body you cannot see at once hides its own control flow, and a long
argument list is almost always a context struct that has not been written yet. Exceeding either is
allowed, but say why in a comment at the definition.

**Carry context in a struct; put the validating on it.** When several steps need the same maps,
market, clock and seats, that is a type. Give it a constructor in the shape of `AccountMaps::new`
and make each step a method, so a step takes the context and its own few arguments and nothing
else. Prefer several small contexts over one wide one: a struct that accumulates every lifetime in
the call graph becomes its own obstacle. `PerpFill` went from 8 lifetimes and 25 fields to 4 and 19
by moving the external-book concern into `ExternalVenue` and the running totals into `FillTally`.

**Name a layer for what it governs, not for what it does to the data.** A chain that read
`fill_perp_order` to `fulfill_perp_order` to `route_and_settle_perp_fill` told a reader nothing:
three synonyms, and each layer had exactly one caller, so the layering carried no reuse either.
Those layers govern the order, the taker's risk limits, and liquidity, and they say so now. If two
functions in a chain could swap names without anybody noticing, the names are wrong.

**One subject per file.** A file is the right size when a reader who opens it finds one subject. A
module root holds the doc, the imports, the `mod` declarations, the re-exports, and only what
several subjects genuinely share. Re-export every public name the old file exported, so a split
changes no caller's imports.

**Some long signatures are deliberate, and stay.** `emit_perp_action_record` keeps its long
parameter list because a struct there makes the caller build the 480-byte record in its own frame
and trips the SBPF stack-overwrite check. `FillerSide` keeps four lifetimes because `&mut &mut` is
invariant. An instruction handler keeps its argument list because that list is the program's ABI.
Each of those carries a comment saying so. Do not "fix" them.

### Verifying a refactor

A refactor that changes behaviour is a rewrite, and the unit test count is the proof it did not:
**the count must be identical before and after.**

- **`cargo check` and `cargo clippy` without `--all-targets` build only the lib.** They report clean
  over a test tree that does not compile. Always pass `--all-targets`.
- **Run each verification command on its own.** Chaining them has produced output that reported
  compile errors at line numbers which did not exist in the file, and has hidden a real `fmt`
  failure that was then reported as clean.
- **Diff the compiler's warning population against a clean `HEAD` worktree**, bucketed by
  `(level, file, message)` so line numbers do not matter. This has caught several real regressions
  that the tests did not, including a settle path reading the raw book instead of the clamped
  ladder a quoter was allocated against, and a shared maker-seat step using the strict position
  lookup where one caller needs the creating one.
- **Measure every function, not the one that improved.** A report that the inner pass reached 155
  lines was true and useless while its three siblings sat at 571, 196 and 331.
- **Verify a re-export by compiling it.** Generate a temporary module that imports every public
  item of the old file by path, compile it under the feature flavors that include the gated names
  and through the path `lib.rs` actually uses, then delete it. Reading the `pub use` list proves
  nothing.

### Refactoring against an audit

A split moves code, so it destroys the feature diff. An auditor reading `master..<branch>` for a
file that was split sees the file deleted and new files appear, with the branch's own changes
scattered inside them. Weigh that before restructuring a file the branch already changed: the cost
is not the new code, it is the diff that can no longer be read. Land readability work only on files that are majorly changed or created in the feature. For example, if you only slightly change liquidation, you should not break up the entire file into modules. But if you overhaul orders, you break it into modules.

### Rust style

Prefer declarative iterator chains (`map`/`filter`/`fold`/`try_fold`/`collect`) over imperative `for`/`while` loops wherever the two are performance-equivalent. Explicit loops are fine when they are genuinely better: hot paths where the imperative form saves real work, or indexed mutation across parallel structures that the borrow checker won't allow through closures. Also avoid redundant recomputation in loops — hoist or precompute values that don't change (or change predictably) across iterations.

### Doc comments

All modules have doc comments. When making feature or refactor changes, update any module-level doc comments that would be invalidated by the change.

**No plan codenames in comments.** A comment must never point at a design doc, plan, spec section,
work phase, or review round as its justification — not `S1`–`S7` / "the S5 rule", not "Phase 2",
not "the plan settles this", "per the sync log", "as the spec warns", "deferred to a later phase". A
reader has the code, not the plan; those labels expire the moment the doc is renamed, reorganized, or
merged, and they encode nothing a reader can act on. Write the reason itself instead:

```rust
// BAD:  the S5 rule applied to the taker flow
// GOOD: an unfilled place_and_take remainder rests on the book instead of
//       cancelling, so the taker keeps queue position at its limit price
```

Referring to a *named, stable artifact* is fine — a crate (`relay-spec`), a type
(`relay_spec::ConditionV0`), a module path, a durable doc that explains a whole subsystem
(`docs/alignment-and-native-offsets.md`) — because those are things the reader can go read and that
change with the code. `docs/propamm-plan.md` is where S1–S7 are *defined*; that document may use
them, code may not. Local step labels inside one function ("first pass … second pass") are fine too,
as long as they describe that function rather than a project timeline.

**Version new event structs.** An `#[event]`'s discriminator is derived from its struct name, so
adding a field to an existing record silently changes the payload under a discriminator consumers
already decode. New velocity events therefore end in `V0` (e.g. `ProtocolUserWithdrawRecordV0`), and
a field addition ships as `…V1` with its own discriminator rather than mutating the `V0` shape. The
records inherited from upstream Drift keep their unversioned names — don't rename those. Wire every
new event into the SDK's subscriber surface in the same change: `EventMap`, the `eventTypes` default
list, and the `VelocityEvent` union in `packages/sdk/src/events/types.ts`, plus the record's type
mirror in `packages/sdk/src/types.ts`. A type mirror without the `EventMap` entry compiles fine and
is simply never decoded.

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

**One §6 row per branch.** While a branch is unmerged, it gets exactly one §6 row; every later change on the branch folds into that row (and into the branch's §2–§5 prose) in place. An integrator migrates against the branch's final state and never saw its intermediate ones, so rewrite the row to the final surface rather than narrating intra-branch history — a rename or a check that only ever existed inside the branch does not belong in the log. The same rule applies to a design doc's sync/change log: one entry per branch, edited in place.

### Error enum stability

The velocity program's `Error` enum is ABI-stable — on-chain clients identify errors by numeric code. When modifying it:

- **Add** new variants at the **bottom** only, never insert between existing ones.
- **Remove** by marking the variant as deprecated (e.g., `/// @deprecated`) and leaving it in place — do not delete or reorder.

### Oracle usage

**Any time you read an oracle price to drive a value transfer, you must guard its validity — never trust a raw oracle price.** A stale, divergent, or low-confidence oracle can mis-size any amount derived from it (PnL, sweeps, settlements, liquidations, withdrawals). When adding or reviewing code that touches an oracle:

- **Gate on validity before using the price.** Mirror the checks the comparable existing path already applies — e.g. `settle_pnl` runs `validate_market_within_price_band`, then (for curve-update markets) `is_recent_oracle_valid` → `get_price_data_and_validity` → `is_oracle_valid_for_action` / `is_price_divergence_ok_for_settle_pnl`, and requires the AMM to be fresh in the same slot (`is_fresh_at`). If you compute the same kind of value (e.g. `net_user_pnl`) elsewhere, apply the same gates — divergence between two code paths that value the same thing is a bug.
- **Consider which price you should actually be using.** The spot/last oracle price is not always correct. Decide deliberately between the live price, the safe/confidence-bounded price, and the TWAP (`last_oracle_price_twap`, 5min twap, etc.) for the operation at hand — TWAPs resist manipulation for things like price-band and divergence checks; live prices suit immediate settlement once validity is confirmed.
- **Prefer the shared helpers** in `math/oracle.rs` and `state/oracle_map.rs` (`get_price_data_and_validity`, `is_oracle_valid_for_action`, per-market `is_recent_oracle_valid` / `get_max_confidence_interval_multiplier`) over ad-hoc checks, so behavior stays consistent across instructions.
- New `VelocityAction` variants exist precisely so each action can express its own validity tolerance — pick the matching action (or add one) rather than reusing an unrelated one.

### Key design patterns

- Velocity uses a custom native entrypoint (discriminator `[0xFF, 0xFF, 0xFF, 0xFF, opcode]`) for high-frequency keeper instructions that bypass Anchor overhead, alongside the standard Anchor `#[program]` entrypoint.
- `remaining_accounts` is used extensively to pass variable numbers of oracle accounts, spot markets, and maker accounts to instructions.
- Zero-copy account loading (`AccountLoader`) is used for large accounts (User, PerpMarket).
- Feature flags: `mainnet-beta` (production gates), `anchor-test` (enables test helpers), `no-entrypoint`/`cpi` (for SDK dependencies).
