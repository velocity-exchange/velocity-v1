<div align="center">
  <img height="120" src="./assets/velocity-logo.svg" />

  <h1>Velocity Exchange</h1>

  <p>
    <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blueviolet" /></a>
    <a href="https://www.npmjs.com/package/@velocity-exchange/sdk"><img alt="npm" src="https://img.shields.io/npm/v/@velocity-exchange/sdk?label=%40velocity-exchange%2Fsdk&color=blueviolet" /></a>
  </p>
</div>

Velocity Protocol v1 is a Solana perpetuals and spot trading protocol. This monorepo holds the
on-chain programs, the TypeScript and Rust SDKs, the admin CLI, and the deployable keeper and DLOB
services.

Integrating against the protocol? Start with the [SDK guide](./packages/sdk/README.md). If you are
migrating from the Drift SDK, read [docs/DRIFT-TO-VELOCITY.md](./docs/DRIFT-TO-VELOCITY.md) as well.

## Repository map

| Path              | What it is                                                                                                                   |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `programs/`       | On-chain programs: `velocity` (core protocol), `vaults`, `jit-proxy`, plus oracle stubs and integrations used by tests        |
| `packages/`       | Publishable npm libraries: `@velocity-exchange/sdk`, `admin-cli`, `vaults-sdk`, `jit-proxy`                                   |
| `apps/`           | Private deployable services, shipped as Docker images and never to npm: `dlob-server`, `keeper-bots-v2`, `usermap-server`     |
| `rust/`           | A second, separate Cargo workspace: `velocity-rs` (Rust SDK), `keep-rs` (keeper bots), `swift` (tx server)                    |
| `tests/`          | ~70 TypeScript integration tests, run against a local validator or bankrun                                                    |
| `deploy-scripts/` | Devnet build, deploy, wipe and init runbooks. See [deploy-scripts/README.md](./deploy-scripts/README.md)                      |
| `docs/`           | Deep-dive docs. See [Further reading](#further-reading)                                                                       |

The two Cargo workspaces are deliberately separate. The root workspace builds the on-chain programs
for SBF. The `rust/` workspace consumes the program as a host library, with its own lockfile and its
own `rust/target/`, so its split solana 4.2 crate tree never unifies with the SBF build.

## Deployments

| Program     | ID                                             |
| ----------- | ---------------------------------------------- |
| `velocity`  | `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P`  |
| `vaults`    | `vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR`  |
| `jit-proxy` | `J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ` |

## Prerequisites

| Tool                        | Version     | Notes                                                                                                        |
| --------------------------- | ----------- | -------------------------------------------------------------------------------------------------------------- |
| Rust                        | ≥ 1.89      | The Anchor 1.0 MSRV. CI pins 1.91.1 in `RUST_TOOLCHAIN`. Develop on ≥ 1.77 so the 16-byte `u128` alignment guards fire locally |
| Rust nightly (rustfmt only) | any recent  | `rustup toolchain install nightly --component rustfmt`. Formatting only; builds, clippy and tests stay on stable |
| Solana platform-tools       | ≥ v1.54     | The bundled cargo in older versions (1.84 and earlier) cannot parse `edition2024` dependencies. See [Troubleshooting](#troubleshooting) |
| Anchor CLI                  | 1.0.2       | Matches the `anchor-lang` version pinned in the programs                                                       |
| Bun                         | ≥ 1.x       | The only supported JS package manager here, not yarn or npm. The repo pins `bun@1.3.14` in `packageManager`    |

On Apple Silicon, always use the x86_64 cross-compile toolchain, never a native aarch64 one. Native
ARM toolchains break the memory-layout expectations of zero-copy accounts, which must match the
on-chain x86_64 representation:

```bash
rustup default stable-x86_64-apple-darwin
```

macOS also needs the SDK path exported for the platform-tools clang. Add this to your shell profile:

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
```

And upgrade the platform-tools once:

```bash
cargo-build-sbf --tools-version v1.54 --force-tools-install
```

## Quick start

```bash
git clone https://github.com/velocity-exchange/velocity-v1.git && cd velocity-v1

# install ALL workspace deps, once, at the repo root (never inside individual packages)
bun install

# build the program and sync the IDL + types into packages/sdk
bun run program:build

# run the Rust unit tests
cargo test -p velocity

# build the whole TypeScript workspace (turbo, dependency order)
bun run build

# run the full integration suite (~70 files, serial; builds the .so first)
bash test-scripts/run-anchor-tests.sh
```

## Fuzzing

The fuzz harnesses for the program live in [`fuzz/`](./fuzz/README.md), built on
[Crucible](https://github.com/asymmetric-research/crucible). They are a separate set of Cargo
workspaces and never ship on-chain.

```bash
# install the Crucible CLI at the pinned rev
cargo install --git https://github.com/asymmetric-research/crucible \
  --rev daeaa4d4a4e334175c4f171daacc7e177ad2fae0 crucible-fuzz-cli --locked

# host tier (pure math)
crucible run amm-pricing prop_k_conserved_swap --timeout 30

# svm tier (needs a devnet .so from `bun run program:build:devnet`)
crucible run e2e-svm invariant_solvency --release --timeout 60
```

The SVM harnesses read `packages/sdk/src/idl/velocity.json` directly, so a program change needs no
extra sync step. [`fuzz/README.md`](./fuzz/README.md) has the harness list and the details.

## Common tasks

| Task                                          | Command                                                                    |
| --------------------------------------------- | -------------------------------------------------------------------------- |
| Build program + sync IDL/types into the SDK   | `bun run program:build`                                                     |
| Regenerate IDL/types only (fast, no SBF build) | `bun run program:idl` (vaults: `program:idl:vaults`, jit: `program:idl:jit-proxy`) |
| Deployable devnet `.so`                       | `bun run program:build:devnet`                                              |
| Mainnet `.so` (production gates on)           | `bun run program:build:mainnet`                                             |
| Build one TS package + its deps               | `bunx turbo run build --filter=@velocity-exchange/sdk`                      |
| Build the `rust/` workspace                   | `bun run rust:build` (or `cargo check --manifest-path rust/Cargo.toml`)     |
| Rust unit tests                               | `cargo test -p velocity` (add `-- --show-output` for stdout)                |
| One integration test                          | `ts-mocha -t 300000 ./tests/<test_file>.ts`                                 |
| Full integration suite (skip rebuild)         | `bash test-scripts/run-anchor-tests.sh --skip-build`                        |
| SDK unit tests                                | `cd packages/sdk && bun run test:ci` (DLOB: `bun run test:dlob`)            |
| Rust lint/format                              | `bun run fmt:rust && cargo clippy -p velocity` (CI enforces both)           |
| SDK lint/format                               | `cd packages/sdk && bun run prettify:fix && bun run lint`                   |

Three habits save a lot of pain here.

- Format Rust with `bun run fmt:rust`, never with plain `cargo fmt`. `rustfmt.toml` merges all of a
  module's imports into a single `use { ... }` block using nightly-only options. Stable `cargo fmt`
  ignores those options, so imports you add stay unmerged and CI's nightly check fails. The script
  wraps nightly rustfmt across every Rust codebase in the repo: the program workspace, the `rust/`
  workspace, the fuzz crates, and the examples. `bun run fmt:rust:check` verifies without writing.
  Format-on-save is preconfigured in the committed `.vscode/settings.json` (VS Code and Cursor) and
  `.zed/settings.json` (Zed), both of which point rust-analyzer's rustfmt at nightly. Generated code
  (`velocity_idl.rs`) and vendored code are excluded on purpose.
- Never hand-edit generated artifacts. `packages/sdk/src/idl/velocity.json`, its `velocity.ts`
  sibling, and `rust/velocity-rs/crates/src/velocity_idl.rs` are all generated from the Rust
  program. Change the program, then run `bun run program:build`, or `bun run program:idl` for the
  fast path.
- Treat `packages/sdk/src/types.ts` as a hand-maintained mirror of the on-chain structs. When a
  struct, account or event changes in the program, update the mirror in the same change.

## Troubleshooting

**`fatal error: 'assert.h' file not found` during `anchor build` (macOS).**
The platform-tools clang has no built-in macOS SDK path. Run
`export SDKROOT="$(xcrun --show-sdk-path)"`, and put it in your shell profile.

**`feature 'edition2024' is required ... not stabilized in this version of Cargo (1.84.0)`.**
The bundled cargo in older platform-tools cannot parse `edition2024` dependencies. Upgrade once with
`cargo-build-sbf --tools-version v1.54 --force-tools-install`, then verify with
`cargo-build-sbf --version`.

**Runtime panic `Access violation in unknown section at address 0x...` on instructions touching
types you did not change.**
This is almost always stale SBF build artifacts after a `Cargo.lock` change, such as a `cargo update`
or a branch switch onto a different lockfile. The cache key misses some dependency-resolution
changes, and the resulting `.so` reads the wrong offsets. Fix it with:

```bash
rm -rf target/sbpf-solana-solana target/deploy
cargo-build-sbf --tools-version v1.54 -- --features anchor-test
```

**Zero-copy layout or `const_assert_eq!` failures on Apple Silicon.**
You are on a native aarch64 toolchain. Switch with `rustup default stable-x86_64-apple-darwin`.

## Dev container (alternative)

If you would rather not install the toolchain locally, `.devcontainer/` ships a Dockerfile and a
docker-compose file with pinned Rust, Solana and Anchor versions. Use your IDE's "Reopen in
Container", or:

```bash
cd .devcontainer && docker compose up -d && docker compose exec velocity bash
```

## Releases

This monorepo uses [changesets](https://github.com/changesets/changesets) to version and publish the
library packages under `packages/*`. Apps under `apps/*` are `private` and ship as Docker images
built from `docker-info.json` by the `velocity-publish` workflow, not to npm.

1. In a PR that changes a publishable package, run `bun run changeset` and describe the bump.
2. On merge to `master`, the `changesets` workflow opens or updates a "Version Packages" PR that
   runs `changeset version`, bumping versions and writing CHANGELOGs. Merge it to commit the bumps.
3. Push a per-package tag `npm-<pkg>-v<version>`. The `npm-publish` workflow builds and publishes
   that package through npm OIDC trusted publishing. It is idempotent, and skips the publish if that
   version is already on the registry. `<pkg>` is the directory name under `packages/`:

| Package                         | Tag example             |
| ------------------------------- | ----------------------- |
| `@velocity-exchange/sdk`        | `npm-sdk-v0.2.3`        |
| `@velocity-exchange/admin-cli`  | `npm-cli-admin-v0.2.3`  |
| `@velocity-exchange/vaults-sdk` | `npm-vaults-sdk-v0.2.3` |
| `@velocity-exchange/jit-proxy`  | `npm-jit-proxy-v0.2.3`  |

The tag version must match the `package.json` version committed by the "Version Packages" PR. Do not
edit `package.json` versions by hand; changesets and the bot own those fields.

## Further reading

| Doc                                                                          | What's in it                                                                 |
| ---------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| [ARCHITECTURE.md](./ARCHITECTURE.md)                                          | Execution flow maps, module responsibility matrix, SDK to program mappings    |
| [docs/DRIFT-TO-VELOCITY.md](./docs/DRIFT-TO-VELOCITY.md)                      | The canonical record of every change vs upstream Drift. Read it if you are integrating |
| [docs/FEES.md](./docs/FEES.md)                                                | The fee architecture: per-fill splits, fee ledger, sweeps, carveouts          |
| [deploy-scripts/README.md](./deploy-scripts/README.md)                        | The devnet upgrade runbook: two-phase buffer deploys, wipe and reinit         |
| [docs/alignment-and-native-offsets.md](./docs/alignment-and-native-offsets.md) | Zero-copy struct alignment invariants. Read before adding fields to accounts  |
| [docs/ACCOUNT-EXTENSION.md](./docs/ACCOUNT-EXTENSION.md)                      | Growing zero-copy accounts past their padding: the `extend_account` crank, the migration runbook, the client rules |
| [docs/EXTERNAL-DEPENDENCIES.md](./docs/EXTERNAL-DEPENDENCIES.md)             | Every external dependency of the on-chain programs. CPI targets, oracles, whitelisted venues, and the full transitive crate graph, each with its trust assumption and failure mode |

## Security and bug bounty

Do not open a GitHub issue to report a vulnerability. Email security@velocity.exchange, and see
[SECURITY.md](./SECURITY.md).

The bug bounty program's severity tiers, payouts, and scope are documented at
[docs.velocity.exchange/protocol/risk-and-safety/bug-bounty](https://docs.velocity.exchange/protocol/risk-and-safety/bug-bounty).
