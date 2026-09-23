# Velocity fuzz harnesses (Crucible)

Property- and invariant-based fuzzing of the `velocity` program using
[Crucible](https://github.com/asymmetric-research/crucible) (LibAFL-backed),
plus LiteSVM for the end-to-end tier. This directory is a set of standalone
Cargo workspaces, one per harness crate, excluded from the repo-root workspace
(see the root `Cargo.toml`) so its `solana-sdk` 3.x / LibAFL / Crucible
dependency trees never unify with the on-chain (SBF) program build. There is no
`fuzz/Cargo.toml` root.

Nothing here ships on-chain. The only program-side hook is the off-by-default
`fuzz-fixtures` cargo feature on `programs/velocity`, which exposes
`pub mod test_utils` to host crates. No SBF, mainnet, or devnet build enables it.

## Layout

| Path | What |
| --- | --- |
| `velocity-fuzz-common/` | Shared re-exports and reusable invariant assertions (some still stubbed, see below) |
| `amm-pricing/`, `funding/`, `margin-liq/`, `oracle/`, `orders-matching/`, `spot/`, `fees-if-bankruptcy/` | Host tier: calls `velocity` math and controller functions directly (no `.so`) and asserts pure properties. Roughly 1.1k exec/s |
| `e2e-svm/`, `e2e-svm-liq/`, `e2e-svm-pause/`, `e2e-svm-revshare/`, `e2e-svm-signedmsg/` | SVM tier: loads the compiled `.so` into LiteSVM, drives real instructions, and reconciles on-chain state against invariants |
| `rust-toolchain.toml` | Pins the toolchain (matches the repo `RUST_TOOLCHAIN`, x86_64 host target) |

## Prerequisites

- The pinned toolchain, installed automatically from `fuzz/rust-toolchain.toml`.
  You need Rust ≥ 1.77 on an x86_64 host so zero-copy `u128` alignment matches
  on-chain. On Apple Silicon, run `rustup override set 1.91.1-x86_64-apple-darwin`
  inside `fuzz/`.
- The Crucible CLI, at the pinned rev:
  ```bash
  cargo install --git https://github.com/asymmetric-research/crucible \
    --rev daeaa4d4a4e334175c4f171daacc7e177ad2fae0 crucible-fuzz-cli --locked
  ```
- SVM tier only: a compiled `target/deploy/velocity.so`. Build it with the
  devnet feature flavor so its account layouts match the host fixtures:
  ```bash
  bun run program:build:devnet     # from the repo root
  ```
  `e2e-svm` and `e2e-svm-revshare` also load the CLOB book program, because
  every perp fill routes through the market's book:
  ```bash
  bun run program:build:clob       # writes anchor-v2/target/deploy/clob.so
  ```

## Running

Each harness "test" is a cargo feature on its crate. List and run:

```bash
crucible list amm-pricing
crucible run amm-pricing prop_k_conserved_swap --timeout 30
crucible run e2e-svm invariant_solvency --release --timeout 60
```

`crucible run <crate> <feature>` builds `--features <feature>` and fuzzes it.
Useful flags: `--timeout <secs>`, `-j <cores>`, `--release`, `--coverage`
(single-core), `--corpus-in/--corpus-out <dir>`. Replay a crash with
`crucible show <crate> <feature>`; minimize with `crucible tmin` / `cmin`.

Harness features come in two kinds:

- `prop_*` / `inv_*`: open-ended properties and invariants. These are what the
  nightly campaign fuzzes, and they are where new bugs get found.
- `regr_<NNN>_*`: regression harnesses that reproduce a specific audit fix,
  where `<NNN>` is the PR number. A `regr_` target crashes on the pre-fix
  program and passes once the fix is in. A regression that only reproduces in
  stateful controller logic belongs at the SVM tier. Do not add a host-tier
  `regr_` for a controller-ordering bug. Assert the underlying math property as
  a `prop_` instead, the way `prop_borrow_debt_monotonic_in_index` does. A host
  math check that the fix does not touch is either vacuous or noise, never a
  real regression.

## IDL

The SVM harnesses embed the program IDL via

```rust
crucible_idl_gen::declare_fuzz_program!(velocity_idl = "../../packages/sdk/src/idl/velocity.json");
```

The path is relative to the crate's `Cargo.toml`, so every harness reads the one
canonical artifact that the TypeScript SDK also consumes. There is no vendored
copy to keep in sync. `bun run program:idl` regenerates that file, and the next
`cargo build` of a harness picks it up.

The harness crates already take `programs/velocity` as a path dependency, so
they build only inside a full repo checkout. Reading the SDK artifact across the
workspace boundary adds no new requirement.

## Writing a sound harness

A harness finds nothing that its assertions do not describe. Check a new one
against these five failure modes, each of which has already bitten a harness
here.

1. **Not tautological.** Do not assert a function's own construction back at it,
   such as asserting that a value is bracketed by bounds derived from that same
   value, or that a monotone function is monotone. Assert a joint or independent
   relationship that a bug could actually break.
2. **Reaches the target.** Confirm that the fixture state can get past the early
   guards into the logic you mean to test. A victim with `base == 0` never
   reaches the liquidation math, and post-only-no-cross orders never produce a
   fill. Prefer confirming reachability with `--coverage`.
3. **Observes the real effect.** A `regr_` should read the actual mutated state
   or assert a fix-exclusive error code. Do not infer the bug from an unrelated
   downstream failure or from a generic pre-existing error code.
4. **Fails loudly on decode drift.** Reading zero-copy accounts must panic, not
   silently skip the invariant, when an existing account is the wrong size. A
   wrong size means the host layout drifted from the `.so`. `read_zc` does this.
5. **No false positives.** An invariant that is not always true for the protocol
   generates noise that drowns real crashes. Gate conditional properties on the
   exact precondition, and mind the precision scales:
   `LIQUIDATION_FEE_PRECISION` is 1e6 while `MARGIN_PRECISION` is 1e4.

## CI

`fuzz.yml` has three jobs:

- `fuzz-build` is the gating job. It runs on PRs that touch `fuzz/**` or the
  program hooks, and compiles every harness feature of every crate. It does not
  run the fuzzer, which keeps the required check fast and deterministic.
- `fuzz-smoke` runs on `workflow_dispatch` only and is `continue-on-error`. It
  installs the pinned Crucible CLI and runs short host-tier invariant smokes.
  It never runs on PRs, so it cannot gate a merge.
- `bundle-manifest` needs no toolchain. It asserts that `fuzz/manifest.fc.json`
  still matches `fuzz/bundle/targets.txt`, and that the bundle, the nightly and
  the devnet script all build the same program flavor.

`fuzz-nightly.yml` is scheduled plus manual, non-gating, and `continue-on-error`.
It runs the `prop_*`/`inv_*` discovery harnesses. Crashes surface as uploaded
artifacts and job-summary lines rather than a red required check, because the
fuzzer explores rather than verifies, and a timeout that found nothing is not a
pass.
