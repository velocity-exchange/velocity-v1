<div align="center">
  <img height="120" src="./assets/velocity-logo.svg" />

  <h1>Velocity Exchange</h1>

  <p>
    <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blueviolet" /></a>
    <a href="https://www.npmjs.com/package/@velocity-exchange/sdk"><img alt="npm" src="https://img.shields.io/npm/v/@velocity-exchange/sdk?label=%40velocity-exchange%2Fsdk&color=blueviolet" /></a>
  </p>
</div>

Velocity Protocol v1: a Solana perpetuals and spot trading protocol. This monorepo holds the
on-chain programs, the TypeScript and Rust SDKs, the admin CLI, and the deployable keeper and DLOB
services.

To integrate against the protocol, start with the [SDK guide](./packages/sdk/README.md). To migrate
from the Drift SDK, read [docs/DRIFT-TO-VELOCITY.md](./docs/DRIFT-TO-VELOCITY.md).

## Repository map

| Path              | What it is                                                                                                                   |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `programs/`       | On-chain programs: `velocity` (core protocol), `vaults`, plus the oracle type stubs the tests use                             |
| `packages/`       | Publishable npm libraries: `@velocity-exchange/sdk`, `admin-cli`, `vaults-sdk`                                                |
| `apps/`           | Private deployable services, shipped as Docker images and never to npm: `dlob-server`, `keeper-bots-v2`, `usermap-server`     |
| `rust/`           | A **second, separate Cargo workspace**: `velocity-rs` (Rust SDK), `keep-rs` (keeper bots), `swift` (tx server)                |
| `tests/`          | About 70 TypeScript integration tests, on a local validator or bankrun                                                        |
| `deploy-scripts/` | Devnet build, deploy, wipe and init runbooks. See [deploy-scripts/README.md](./deploy-scripts/README.md)                      |
| `docs/`           | Reference docs. See [Further reading](#further-reading)                                                                       |

The two Cargo workspaces are separate on purpose. The root workspace builds the on-chain programs
for SBF. The `rust/` workspace consumes the program as a host library, with its own lockfile and its
own `rust/target/`, so its split solana 4.2 crate tree never unifies with the SBF build.

## Deployments

| Program     | ID                                             |
| ----------- | ---------------------------------------------- |
| `velocity`  | `vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P`  |
| `vaults`    | `vAuLTsyrvSfZRuRB3XgvkPwNGgYSs9YRYymVebLKoxR`  |

## Prerequisites

| Tool                       | Version                | Notes                                                                                                     |
| -------------------------- | ---------------------- | --------------------------------------------------------------------------------------------------------- |
| Rust                       | **≥ 1.89**             | Anchor 1.0 MSRV. Develop with ≥ 1.77 so the 16-byte `u128` alignment guards are exercised locally          |
| Rust nightly (rustfmt only) | any recent             | `rustup toolchain install nightly --component rustfmt`. Formatting only. Builds, clippy and tests stay on stable |
| Solana platform-tools      | **≥ v1.54**            | The cargo bundled with older tools (1.84 and below) cannot parse `edition2024` dependencies. See [Troubleshooting](#troubleshooting) |
| Anchor CLI                 | **1.0.2**              | Matches the `anchor-lang` version pinned in the programs                                                    |
| Bun                        | ≥ 1.x                  | The only supported JavaScript package manager here. Do not use yarn or npm                                  |

**On Apple Silicon, always use the x86_64 cross-compile toolchain, never a native aarch64 one.** A
native ARM toolchain breaks the memory-layout expectations of zero-copy accounts, which must match
the on-chain x86_64 representation:

```bash
rustup default stable-x86_64-apple-darwin
```

macOS also needs the SDK path exported for the platform-tools clang. Add it to your shell profile:

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
```

Upgrade the platform-tools once:

```bash
cargo-build-sbf --tools-version v1.54 --force-tools-install
```

## Quick start

```bash
git clone https://github.com/velocity-exchange/velocity-v1.git && cd velocity-v1

# install every workspace dependency, once, at the repo root (never inside an individual package)
bun install

# build the program and sync the IDL and types into packages/sdk
bun run program:build

# run the Rust unit tests
cargo test -p velocity

# build the whole TypeScript workspace (turbo, in dependency order)
bun run build

# run the full integration suite (about 70 files, serial; builds the .so first)
bash test-scripts/run-anchor-tests.sh
```

## Fuzzing

The program's fuzz harnesses are in [`fuzz/`](./fuzz/README.md), built on
[Crucible](https://github.com/asymmetric-research/crucible). They run as a separate Cargo workspace
and never ship on-chain.

```bash
# install the Crucible CLI (pinned rev in fuzz/README.md)
cargo install --git https://github.com/asymmetric-research/crucible crucible-fuzz-cli --locked

# host tier (pure math)
crucible run amm-pricing prop_k_conserved_swap --timeout 30

# svm tier (needs a devnet .so from `bun run program:build:devnet`)
crucible run e2e-svm invariant_solvency --release --timeout 60
```

The SVM harnesses read `packages/sdk/src/idl/velocity.json` directly, so a program change needs no
extra sync step. [`fuzz/README.md`](./fuzz/README.md) lists the harnesses.

## Common tasks

| Task                                          | Command                                                                    |
| --------------------------------------------- | -------------------------------------------------------------------------- |
| Build program and sync IDL/types into the SDK | `bun run program:build`                                                     |
| Regenerate IDL/types only (fast, no SBF build) | `bun run program:idl` (vaults: `program:idl:vaults`)                       |
| Deployable devnet `.so`                       | `bun run program:build:devnet`                                              |
| Mainnet `.so` (production gates on)           | `bun run program:build:mainnet`                                             |
| Build one TS package and its deps             | `bunx turbo run build --filter=@velocity-exchange/sdk`                      |
| Build the `rust/` workspace                   | `bun run rust:build` (or `cargo check --manifest-path rust/Cargo.toml`)     |
| Rust unit tests                               | `cargo test -p velocity` (add `-- --show-output` for stdout)                |
| One integration test                          | `ts-mocha -t 300000 ./tests/<test_file>.ts`                                 |
| Full integration suite (skip rebuild)         | `bash test-scripts/run-anchor-tests.sh --skip-build`                        |
| SDK unit tests                                | `cd packages/sdk && bun run test:ci` (DLOB: `bun run test:dlob`)            |
| Rust lint and format                          | `bun run fmt:rust && cargo clippy -p velocity` (CI enforces both)           |
| SDK lint and format                           | `cd packages/sdk && bun run prettify:fix && bun run lint`                   |

Three rules worth knowing before you start:

- **Format Rust with `bun run fmt:rust`, never with plain `cargo fmt`.** `rustfmt.toml` merges all
  of a module's imports into one `use { ... }` block through nightly-only options. Stable
  `cargo fmt` ignores those options, so imports you add stay unmerged and the nightly check in CI
  fails. The script wraps nightly rustfmt across every Rust codebase in the repo: the program
  workspace, the `rust/` workspace, the fuzz crates, and the examples. `bun run fmt:rust:check`
  verifies without writing. Format-on-save is already configured by the committed
  `.vscode/settings.json` (VS Code and Cursor) and `.zed/settings.json` (Zed), which point
  rust-analyzer's rustfmt at nightly. Generated code (`velocity_idl.rs`) and vendored code are
  excluded on purpose.
- **Never hand-edit a generated artifact.** `packages/sdk/src/idl/velocity.json`, its `velocity.ts`
  companion, and `rust/velocity-rs/crates/src/velocity_idl.rs` are generated from the Rust program.
  Change the program, then run `bun run program:build`, or `program:idl` for the fast path.
- **`packages/sdk/src/types.ts` is a hand-maintained mirror of the on-chain structs.** Whenever a
  struct, account or event changes in the program, update the mirror in the same change.

## Troubleshooting

**`fatal error: 'assert.h' file not found` during `anchor build` on macOS.**
The platform-tools clang has no built-in macOS SDK path. Run
`export SDKROOT="$(xcrun --show-sdk-path)"`, and put it in your shell profile.

**`feature 'edition2024' is required ... not stabilized in this version of Cargo (1.84.0)`.**
The cargo bundled with older platform-tools cannot parse `edition2024` dependencies. Upgrade once
with `cargo-build-sbf --tools-version v1.54 --force-tools-install`, then verify with
`cargo-build-sbf --version`.

**Runtime panic `Access violation in unknown section at address 0x...` on instructions that touch
types you did not change.**
This is almost always stale SBF build artifacts after a `Cargo.lock` change, such as a `cargo
update` or a branch switch to a different lockfile. The cache key misses some dependency-resolution
changes, and the resulting `.so` reads wrong offsets. Fix it with:

```bash
rm -rf target/sbpf-solana-solana target/deploy
cargo-build-sbf --tools-version v1.54 -- --features anchor-test
```

**Unexplained zero-copy layout failures or `const_assert_eq!` failures on Apple Silicon.**
The toolchain is a native aarch64 one. Switch with `rustup default stable-x86_64-apple-darwin`.

## Dev container (alternative)

To avoid installing the toolchain locally, use `.devcontainer/`, which ships a Dockerfile and a
docker-compose file with pinned Rust, Solana and Anchor versions. Use your IDE's "Reopen in
Container", or:

```bash
cd .devcontainer && docker compose up -d && docker compose exec velocity bash
```

## Releases

This monorepo uses [changesets](https://github.com/changesets/changesets) to version and publish the
library packages under `packages/*`. The apps under `apps/*` are `private` and ship as Docker images
(see `docker-info.json` and `docker-on-tag.yml`), not to npm.

Workflow:

1. In a PR that changes a publishable package, run `bun run changeset` and describe the bump.
2. On merge to `master`, the `changesets` workflow opens or updates a **Version Packages** PR that
   runs `changeset version`, which bumps the versions and writes the CHANGELOGs. Merge it to commit
   the bumps.
3. Push a per-package tag `npm-<pkg>-v<version>`. The `npm-publish` workflow builds and publishes
   that package through npm OIDC trusted publishing. It is idempotent and skips the publish if that
   version is already on the registry. `<pkg>` is the directory name under `packages/`:

| Package                         | Tag example             |
| ------------------------------- | ----------------------- |
| `@velocity-exchange/sdk`        | `npm-sdk-v0.2.3`        |
| `@velocity-exchange/admin-cli`  | `npm-cli-admin-v0.2.3`  |
| `@velocity-exchange/vaults-sdk` | `npm-vaults-sdk-v0.2.3` |

The tag version must match the `package.json` version committed by the Version Packages PR. Do not
edit a `package.json` version by hand. Changesets and the bot own those fields.

## Further reading

| Doc                                                                          | What it covers                                                               |
| ---------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| [ARCHITECTURE.md](./ARCHITECTURE.md)                                          | Execution flow maps, the module responsibility matrix, SDK to program mappings |
| [docs/DRIFT-TO-VELOCITY.md](./docs/DRIFT-TO-VELOCITY.md)                      | The record of every change against upstream Drift. Read it if you integrate   |
| [FEES.md](./FEES.md)                                                          | The fee architecture: per-fill splits, fee ledger, sweeps, carveouts          |
| [deploy-scripts/README.md](./deploy-scripts/README.md)                        | Devnet upgrade runbook: two-phase buffer deploys, wipe and reinit             |
| [docs/clob-client-integration.md](./docs/clob-client-integration.md)          | Placing, cancelling, modifying and displaying orders that rest on a CLOB      |
| [docs/clob-client-surface.md](./docs/clob-client-surface.md)                  | The design behind the CLOB client surface, and what is still open             |
| [docs/alignment-and-native-offsets.md](./docs/alignment-and-native-offsets.md) | Zero-copy struct alignment invariants. Read before adding fields to accounts  |
| [docs/ACCOUNT-EXTENSION.md](./docs/ACCOUNT-EXTENSION.md)                      | Growing zero-copy accounts past their padding: the `extend_account` crank, the migration runbook, the client rules |
| [docs/EXTERNAL-DEPENDENCIES.md](./docs/EXTERNAL-DEPENDENCIES.md)             | Every external dependency of the on-chain programs, with trust assumptions and failure modes: CPI targets, oracles, whitelisted venues, and the full transitive crate graph |

## Security and bug bounty

**Do not open a GitHub issue for a vulnerability.** Email security@velocity.exchange. See
[SECURITY.md](./SECURITY.md).

The bug bounty program's severity tiers, payouts and scope are documented at
[docs.velocity.exchange/protocol/risk-and-safety/bug-bounty](https://docs.velocity.exchange/protocol/risk-and-safety/bug-bounty).
