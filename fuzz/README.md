# Velocity fuzz harnesses (Crucible)

Property- and invariant-based fuzzing of the `velocity` program with
[Crucible](https://github.com/asymmetric-research/crucible), which is LibAFL-backed, plus
LiteSVM for the end-to-end tier. This directory is a **set of standalone Cargo
workspaces**, one per harness crate. The root `Cargo.toml` excludes them on purpose, so
their `solana-sdk` 3.x, LibAFL and Crucible dependency trees never unify with the
on-chain SBF program build. There is no `fuzz/Cargo.toml` root.

Nothing here ships on-chain. The only program-side hook is the off-by-default
`fuzz-fixtures` cargo feature on `programs/velocity`, which exposes `pub mod test_utils`
to host crates. No SBF, mainnet or devnet build ever enables it.

## Layout

| Path | What |
| --- | --- |
| `velocity-fuzz-common/` | Shared re-exports and reusable invariant assertions. Some are still stubbed, see below |
| `amm-pricing/`, `funding/`, `margin-liq/`, `oracle/`, `orders-matching/`, `spot/`, `fees-if-bankruptcy/` | **Host tier**: calls `velocity` math and controller functions directly, with no `.so`, and asserts pure properties. About 1,100 executions per second |
| `e2e-svm/`, `e2e-svm-liq/`, `e2e-svm-pause/`, `e2e-svm-revshare/`, `e2e-svm-signedmsg/` | **SVM tier**: loads the compiled `.so` into LiteSVM, drives real instructions, and reconciles on-chain state against the invariants |
| `rust-toolchain.toml` | Pins the toolchain. It matches the repo `RUST_TOOLCHAIN` and the x86_64 host target |

## Prerequisites

- The pinned toolchain, which `fuzz/rust-toolchain.toml` installs. Use Rust 1.77 or later
  on an **x86_64** host so zero-copy `u128` alignment matches on-chain. On Apple Silicon,
  run `rustup override set 1.91.1-x86_64-apple-darwin` inside `fuzz/`.
- The Crucible CLI, at the pinned rev:
  ```bash
  cargo install --git https://github.com/asymmetric-research/crucible \
    --rev daeaa4d4a4e334175c4f171daacc7e177ad2fae0 crucible-fuzz-cli --locked
  ```
- **SVM tier only**: a compiled `target/deploy/velocity.so`. Build it with the
  devnet feature flavor so its account layouts match the host fixtures:
  ```bash
  bun run program:build:devnet     # from the repo root
  ```

## Running

Each harness "test" is a cargo feature on its crate. List and run:

```bash
crucible list amm-pricing
crucible run amm-pricing prop_k_conserved_swap --timeout 30
crucible run e2e-svm invariant_solvency --release --timeout 60
```

`crucible run <crate> <feature>` builds `--features <feature>` and fuzzes it.
The useful flags are `--timeout <secs>`, `-j <cores>`, `--release`, `--coverage`, which is
single-core, and `--corpus-in` or `--corpus-out <dir>`. Replay a crash with
`crucible show <crate> <feature>`. Minimize with `crucible tmin` or `cmin`.

Harness features come in two kinds:

- `prop_*` and `inv_*` are open-ended **properties and invariants**. These are the
  discovery surface, and the nightly campaign fuzzes them.
- `regr_<NNN>_*` are **regression** harnesses that reproduce one audit fix, where `<NNN>`
  is the PR number. A `regr_` target crashes on the pre-fix program and passes once the
  fix is in. A regression that only reproduces in stateful controller logic belongs at the
  SVM tier. Do **not** add a host-tier `regr_` for a controller-ordering bug. Assert the
  underlying math property as a `prop_` instead, as
  `prop_borrow_debt_monotonic_in_index` does. A host math check that the fix does not
  touch is either vacuous or noise, never a real regression.

## IDL

The SVM harnesses embed the program IDL via

```rust
crucible_idl_gen::declare_fuzz_program!(velocity_idl = "../../packages/sdk/src/idl/velocity.json");
```

The path is relative to the crate's `Cargo.toml`, so every harness reads the one
canonical artifact that the TypeScript SDK also consumes. There is no vendored
copy to keep in sync. `bun run program:idl` regenerates that file, and the next
`cargo build` of a harness picks it up.

The harness crates already take `programs/velocity` as a path dependency, so they
build only inside a full repo checkout. Reading the SDK artifact across the
workspace boundary adds no new requirement.

## Writing a sound harness

The campaign is only as good as its assertions. Before you add one, check it against these
failure modes. Each one has caught out a harness here.

1. **Not tautological.** Do not assert a function's own construction back at it, such as
   that a value is bracketed by bounds derived from that same value, or that a monotone
   function is monotone. Assert a *joint* or *independent* relationship that a bug could
   break.
2. **Reaches the target.** Make sure the fixture state can get past the early guards into
   the logic you mean to test. A victim with `base == 0` never reaches the liquidation
   math, and a post-only order that does not cross never produces a fill. Confirm
   reachability with `--coverage`.
3. **Observes the real effect.** A `regr_` target must observe the mutated state itself, or
   assert a **fix-exclusive** error code. It must not infer the bug from an unrelated
   downstream failure or from a generic pre-existing error code.
4. **Fails loudly on decode drift.** A read of a zero-copy account must panic, rather than
   skip the invariant, when an existing account is the wrong size. A wrong size means the
   host layout drifted from the `.so`. `read_zc` panics.
5. **No false positives.** An invariant that is not *always* true for the protocol
   generates noise that drowns real crashes. Gate a conditional property on the exact
   precondition, and mind the precision scales. `LIQUIDATION_FEE_PRECISION` is 1e6 and
   `MARGIN_PRECISION` is 1e4.

## CI

- `fuzz.yml` gates PRs that touch `fuzz/**` or the program hooks. It compiles every harness
  feature. It does **not** run the fuzzer, so the required check stays fast and
  deterministic.
- `fuzz-nightly.yml` runs on a schedule and on demand. It is **non-gating** and
  `continue-on-error`. It runs the `prop_*` and `inv_*` discovery harnesses. A crash shows
  up as an uploaded artifact and a job-summary line, never as a red required check. A
  fuzzer is a discovery tool rather than a pass/fail gate.
