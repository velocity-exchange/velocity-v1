<div align="center">
  <img height="120" src="./assets/velocity-logo.svg" />

  <h1>Velocity Exchange</h1>

  <p>
    <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blueviolet" /></a>
    <a href="https://www.npmjs.com/package/@velocity-exchange/sdk"><img alt="npm" src="https://img.shields.io/npm/v/@velocity-exchange/sdk?label=%40velocity-exchange%2Fsdk&color=blueviolet" /></a>
  </p>
</div>

Velocity Protocol v1: a Solana perpetuals and spot trading protocol. This monorepo holds the
on-chain programs, the TypeScript and Rust SDKs, the admin CLI, and the deployable keeper/DLOB
services.

Integrating against the protocol? Start with the [SDK guide](./packages/sdk/README.md) and, if you
are migrating from the Drift SDK, [docs/DRIFT-TO-VELOCITY.md](./docs/DRIFT-TO-VELOCITY.md).

## Repository map

| Path              | What it is                                                                                                                   |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `programs/`       | On-chain programs: `velocity` (core protocol), `vaults`, `jit-proxy`, plus oracle stubs/integrations used by tests            |
| `packages/`       | Publishable npm libraries: `@velocity-exchange/sdk`, `admin-cli`, `vaults-sdk`, `jit-proxy`                                   |
| `apps/`           | Private deployable services (shipped as Docker images, never npm): `dlob-server`, `keeper-bots-v2`, `usermap-server`          |
| `rust/`           | A **second, separate Cargo workspace**: `velocity-rs` (Rust SDK), `keep-rs` (keeper bots), `swift` (tx server)                |
| `tests/`          | ~70 TypeScript integration tests (local validator / bankrun)                                                                  |
| `deploy-scripts/` | Devnet build/deploy/wipe/init runbooks; see [deploy-scripts/README.md](./deploy-scripts/README.md)                           |
| `docs/`           | Deep-dive docs (see [Further reading](#further-reading))                                                                      |

The two Cargo workspaces are deliberately separate: the root workspace builds the on-chain
programs (SBF), while `rust/` consumes the program as a host library with its own lockfile and
`rust/target/`, so its solana-sdk 3.x tree never unifies with the SBF build.

## Deployments

| Program     | ID                                             |
| ----------- | ---------------------------------------------- |
| `velocity`  | `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P`  |
| `vaults`    | `vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR`  |
| `jit-proxy` | `J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ` |

## Prerequisites

| Tool                       | Version                | Notes                                                                                                     |
| -------------------------- | ---------------------- | --------------------------------------------------------------------------------------------------------- |
| Rust                       | **≥ 1.89**             | Anchor 1.0 MSRV. Develop with ≥ 1.77 so the 16-byte `u128` alignment guards are exercised locally          |
| Solana platform-tools      | **≥ v1.54**            | Older bundled cargo (≤ 1.84) cannot parse `edition2024` dependencies; see [Troubleshooting](#troubleshooting) |
| Anchor CLI                 | **1.0.2**              | Matches the `anchor-lang` version pinned in the programs                                                    |
| Bun                        | ≥ 1.x                  | The only supported JS package manager here (not yarn/npm)                                                   |

**Apple Silicon (M-series): always use the x86_64 cross-compile toolchain, never a native aarch64
one.** Native ARM toolchains break the memory-layout expectations of zero-copy accounts, which must
match the on-chain (x86_64) representation:

```bash
rustup default stable-x86_64-apple-darwin
```

macOS also needs the SDK path exported for the platform-tools clang (add to your shell profile):

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
```

And upgrade the platform-tools once:

```bash
cargo-build-sbf --tools-version v1.54 --force-tools-install
```

## Quick start

```bash
bash test-scripts/run-anchor-tests.sh
```

## Fuzzing

Fuzz harnesses for the program live in [`fuzz/`](./fuzz/README.md), built on [Crucible](https://github.com/asymmetric-research/crucible). They run as a separate Cargo workspace and don't ship on-chain.

```bash
# install the Crucible CLI (pinned rev in fuzz/README.md)
cargo install --git https://github.com/asymmetric-research/crucible crucible-fuzz-cli --locked

# host tier (pure math)
crucible run amm-pricing prop_k_conserved_swap --timeout 30

# svm tier (needs a devnet .so from `bun run program:build:devnet`)
crucible run e2e-svm invariant_solvency --release --timeout 60
```

After a program change, run `bash fuzz/sync-idls.sh` to re-sync the vendored IDL. See [`fuzz/README.md`](./fuzz/README.md) for the harness list and details.

# Development (with devcontainer)
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
| Rust lint/format                              | `cargo fmt && cargo clippy -p velocity` (CI enforces both)                  |
| SDK lint/format                               | `cd packages/sdk && bun run prettify:fix && bun run lint`                   |

Two rules that save a lot of pain:

- **Never hand-edit generated artifacts.** `packages/sdk/src/idl/velocity.json`/`velocity.ts` and
  `rust/velocity-rs/crates/src/velocity_idl.rs` are all generated from the Rust program. Change the
  program, then `bun run program:build` (or `program:idl` for the fast path).
- **`packages/sdk/src/types.ts` is a hand-maintained mirror of the on-chain structs.** Whenever a
  struct/account/event changes in the program, update the mirror in the same change.

## Troubleshooting

**`fatal error: 'assert.h' file not found` during `anchor build` (macOS).**
The platform-tools clang has no built-in macOS SDK path. Fix:
`export SDKROOT="$(xcrun --show-sdk-path)"` (put it in your shell profile).

**`feature 'edition2024' is required ... not stabilized in this version of Cargo (1.84.0)`.**
The bundled cargo in older platform-tools can't parse `edition2024` dependencies. Upgrade once with
`cargo-build-sbf --tools-version v1.54 --force-tools-install`, then verify via
`cargo-build-sbf --version`.

**Runtime panic `Access violation in unknown section at address 0x...` on instructions touching
types you didn't change.**
Almost always stale SBF build artifacts after a `Cargo.lock` change (e.g. after `cargo update` or
switching branches with different lockfiles). The cache key misses some dep-resolution changes and
the resulting `.so` reads wrong offsets. Fix:

```bash
rm -rf target/sbpf-solana-solana target/deploy
cargo-build-sbf --tools-version v1.54 -- --features anchor-test
```

**Weird zero-copy layout/`const_assert_eq!` failures on Apple Silicon.**
You're on a native aarch64 toolchain. Switch: `rustup default stable-x86_64-apple-darwin`.

## Dev container (alternative)

If you'd rather not install the toolchain locally, `.devcontainer/` ships a Dockerfile and
docker-compose with pinned Rust/Solana/Anchor versions. Use your IDE's "Reopen in Container", or:

```bash
cd .devcontainer && docker compose up -d && docker compose exec velocity bash
```

## Releases

This monorepo uses [changesets](https://github.com/changesets/changesets) for versioning and
publishing the library packages under `packages/*`. Apps under `apps/*` are `private` and ship as
Docker images (see `docker-info.json` / `docker-on-tag.yml`), not npm.

Workflow:

1. In a PR that changes a publishable package, run `bun run changeset` and describe the bump.
2. On merge to `master`, the `changesets` workflow opens/updates a **Version Packages** PR that
   runs `changeset version` (bumps versions + writes CHANGELOGs). Merge it to commit the bumps.
3. Push a per-package tag `npm-<pkg>-v<version>`; the `npm-publish` workflow builds and publishes
   that package via npm OIDC trusted publishing (idempotent: skipped if that version is already on
   the registry). `<pkg>` is the directory name under `packages/`:

| Package                         | Tag example             |
| ------------------------------- | ----------------------- |
| `@velocity-exchange/sdk`        | `npm-sdk-v0.2.3`        |
| `@velocity-exchange/admin-cli`  | `npm-cli-admin-v0.2.3`  |
| `@velocity-exchange/vaults-sdk` | `npm-vaults-sdk-v0.2.3` |
| `@velocity-exchange/jit-proxy`  | `npm-jit-proxy-v0.2.3`  |

The tag version must match the `package.json` version committed by the "Version Packages" PR. Do
not manually edit `package.json` versions; changesets and the bot own those fields.

## Further reading

| Doc                                                                          | What's in it                                                                 |
| ---------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| [ARCHITECTURE.md](./ARCHITECTURE.md)                                          | Execution flow maps, module responsibility matrix, SDK ↔ program mappings     |
| [docs/DRIFT-TO-VELOCITY.md](./docs/DRIFT-TO-VELOCITY.md)                      | Canonical record of every change vs upstream Drift; read this if integrating |
| [FEES.md](./FEES.md)                                                          | The fee architecture: per-fill splits, fee ledger, sweeps, carveouts          |
| [deploy-scripts/README.md](./deploy-scripts/README.md)                        | Devnet upgrade runbook (two-phase buffer deploys, wipe/reinit)                |
| [docs/alignment-and-native-offsets.md](./docs/alignment-and-native-offsets.md) | Zero-copy struct alignment invariants; read before adding fields to accounts  |

## Bug bounty

Information about the bug bounty is in [bug-bounty/README.md](./bug-bounty/README.md).
