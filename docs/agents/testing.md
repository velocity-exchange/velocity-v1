# Testing

Read this before you run or change a test suite, the CI workflow, or the test runner scripts.

## Local CI emulation

`bash test-scripts/ci-local.sh` runs the gating checks from `.github/workflows/main.yml` locally.
`--fast` runs static checks only, and `--full` adds the anchor and vault integration suites and the
rust-workspace tests.

Keep `test-scripts/ci-local.sh` in sync with the CI workflow. Whenever a gating job in
`.github/workflows/main.yml` is added, removed, or its command changes, mirror the change in
`ci-local.sh` in the same PR. The script also handles two local-only traps that CI never hits:

- The SBF cache-poisoning guard (see the access-violation entry in [`build.md`](./build.md)). It
  wipes `target/sbpf*-solana-solana` before the integration-suite build, since `.so` files built on
  a cache that mixed feature flavors die at entry with `Access violation in unknown section`.
- The IDL-flavor restore. The anchor suite's own build (default features, so `mainnet-beta` is on)
  syncs an IDL with the devnet-only instructions compiled out into `packages/sdk/src/idl/`.
  Committing that breaks `wipe-devnet.ts`, so after the suites the script reruns
  `bun run program:idl` to restore the canonical flavor.

## Build each program with its own feature flags

`run-vault-tests.sh` builds velocity, vaults and the fixtures separately on purpose. A single
`anchor build -- --no-default-features --features no-entrypoint,anchor-test` applies velocity's
flags to every program. `--no-default-features` strips the vaults, pyth and token_faucet
entrypoints, which produces 896-byte stub `.so` files. Bankrun then reports
`Program is not deployed` and every vault `before` hook fails with
`invalid account data for instruction 2`. Vaults separately needs `--features anchor-test` for the
test `admin::ID` in `constants.rs` that the fee-update tests sign with. Without it `is_admin`
rejects with `0x7d3`.

## Cargo unifies features across workspace members

`cargo test --workspace --features X` turns X on for every member, not just the one you meant. The
offline Rust gate stays clean only because no member enables `rpc_tests`. Live tests are split out
under that feature rather than hidden behind `#[ignore]`.

## App jest suites use swc, not ts-jest

The `apps/*` suites share `jest.config.app.cjs` (@swc/jest plus `jest.setup.app.ts`). The app
sources were written against swc semantics. They need `jest.mock` factory hoisting and must not
type-check at run time, because app sources are not type-clean against the SDK types. ts-jest fails
both ways. `isolatedModules: true` skips mock hoisting, and `isolatedModules: false` blocks on the
type errors. Each wired package has a one-line `jest.config.cjs` re-exporting the preset. The
`ts-tests` CI job lists the app suites explicitly rather than globbing `./apps/*`, so an unwired
app cannot slip into the gate unnoticed.

## Rust unit tests

```bash
cargo test -p velocity                    # velocity program only
cargo test -p velocity -- --show-output  # with stdout
```

## TypeScript integration tests

```bash
ts-mocha -t 300000 ./tests/<test_file>.ts          # one file
bash test-scripts/run-anchor-tests.sh              # build, then every velocity test file
bash test-scripts/run-anchor-tests.sh --skip-build # reuse the built .so
SINGLE_PROCESS=1 bash test-scripts/run-anchor-tests.sh --skip-build  # one mocha process: 53s -> 19s
```

`bash test-scripts/run-anchor-tests.sh --help` lists the flags and the environment variables.

Nearly all files in `tests/` run in-process on LiteSVM through
`packages/sdk/src/litesvm/litesvmConnection.ts`, which presents the SVM to the SDK as a web3.js
`Connection`. A handful use Anchor's local validator instead. The tests import the SDK by relative
path (`../packages/sdk/src/...`) and resolve `@coral-xyz/anchor` and friends from the repo-root
`node_modules`. The single root `bun install` provides both. If deps are missing, run
`bun install` at the repo root.

The SDK build the runner does before the suite starts with `rm -rf lib`, so it is a full tsc every
time, about seven seconds. The runner skips it when nothing under `packages/sdk/src` is newer than
the built entrypoint, and shows a progress line when it does run, because a silent multi-second
pause before the first test reads as a hang.

Those setup lines are printed in the same shape the reporter uses for test files, so the run reads
as one column of results. `deploy-scripts/_ui.sh` is deliberately not reused for them. Its
`run_step` indents six spaces to sit under a `header`, and there is no header in this output.

`SINGLE_PROCESS=1` exists because about 1.6s of each file's 1.8s is importing `packages/sdk/src`,
paid once per process. Each file still gets its own LiteSVM (about 40ms), so the isolation that
matters is kept. It is not the default. One process shares module-level state across files, which
can cause order-dependent flakiness. Use it locally, and leave CI on the per-file gate.

Both modes render through `test-scripts/mocha-file-reporter.cjs`, which prints one line per test
file plus any failures. Mocha's own reporters group by describe block, which prints several hundred
flat lines across a suite this size. In per-file mode each child reports its own file and the
runner adds up the totals, so the two modes print the same thing and differ only in speed.

The reporter prefixes its output with U+0001 so the runner can show the report while sending every
test log to a file it prints only on failure. It emits its totals on a U+0002 line that the
per-file runner consumes without displaying. The markers are needed because tests write to both
stdout and stderr, and no spare file descriptor is available, since ts-mocha spawns mocha as a
child and passes through only fds 0, 1 and 2. Colors follow the same gate as
`deploy-scripts/_ui.sh`, so `--no-color` and `NO_COLOR` behave as they do in the deploy CLIs.

## SDK unit tests

```bash
cd packages/sdk/ && bun run test:dlob    # DLOB tests
cd packages/sdk/ && bun run test:ci      # CI subset
```

## Formatting and lint

```bash
bun run fmt:rust                 # all Rust in the repo (wraps nightly rustfmt, see below)
bun run fmt:rust:check           # verify without writing (what CI enforces)
cargo clippy --workspace --all-targets -- -D warnings  # CI, part 1: every crate, tests included
cargo clippy -p velocity --all-targets --no-default-features --features no-entrypoint,anchor-test -- -D warnings  # CI, part 2
cd packages/sdk/ && bun run prettify:fix  # SDK (TypeScript)
```

Clippy runs with `-D warnings`, so a new warning fails the PR, including warnings in tests. The
second run covers the anchor-test flavor that `bun run program:build` compiles, because code behind
`#[cfg(feature = "mainnet-beta")]` and `#[cfg(feature = "anchor-test")]` differs between the two
builds. A variable used only inside a `mainnet-beta` block belongs inside that block, or the other
flavor warns that it is unused.

Lint settings shared by every program crate live in `[workspace.lints]` in the root `Cargo.toml`,
which each program inherits with `[lints] workspace = true`. Velocity also denies
`clippy::wildcard_enum_match_arm` in `lib.rs`. Fix a warning rather than allowing it. Where an allow
is the right call, put it on the narrowest item and say why in a comment.

Rust formatting requires nightly rustfmt. `rustfmt.toml` sets `imports_granularity = "One"` and
`group_imports = "One"`, which merge all `use` items in a module into a single `use { ... }` block.
Those are nightly-only options. Stable `cargo fmt` warns and ignores them, so newly added imports
stay unmerged and CI's nightly fmt check fails. Install once with
`rustup toolchain install nightly --component rustfmt` and always format through
`bun run fmt:rust` (wraps `scripts/fmt-rust.sh`, which covers every Rust codebase in the repo).
Only formatting uses nightly. Builds, clippy and tests stay on the stable toolchains. CI pins the
exact nightly in `RUST_NIGHTLY_TOOLCHAIN` in `.github/workflows/main.yml`.

The generated `rust/velocity-rs/crates/src/velocity_idl.rs` and `rust/keep-rs/vendor/` are on the
rustfmt ignore list, because codegen pipes through stable rustfmt and vendored code keeps upstream
formatting. Keep them stable-formatted.

CI also enforces the style on the standalone workspaces that `bun run fmt:rust` doesn't reach.
Each `fuzz/<crate>/` is fmt-checked by its `fuzz-build` CI matrix job (format one locally with
`cargo +nightly fmt --manifest-path fuzz/<crate>/Cargo.toml --all`). The non-member
`rust/velocity-rs/examples/*` crates don't resolve under cargo, so the rust-workspace CI job checks
them with raw `rustup run <nightly> rustfmt --edition <crate edition>`.
