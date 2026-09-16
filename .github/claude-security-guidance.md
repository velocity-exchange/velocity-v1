# Velocity Protocol security review guidance

The `anthropics/claude-code-security-review` GitHub Action reads this file as its
false-positive filtering instructions. It records the intentional patterns in the Velocity
codebase that a generic security model tends to flag by mistake. Treat them as expected and
do not report them, unless the change clearly violates the invariant described here.

For deeper context, read `CLAUDE.md`, `ARCHITECTURE.md` and
`docs/alignment-and-native-offsets.md`.

## Intentional patterns, do not flag

### 1. Zero-copy struct layout with explicit padding

The Anchor zero-copy account structs in `programs/velocity/src/state/` use `repr(C)` and
manual padding bytes, so that `(SIZE - 8) % 16 == 0` and every `u128` and `i128` field is
ordered before any `PoolBalance` field. That keeps `sizeof` identical to the onchain SBF
layout whether the host runs Rust 1.76, where `align_of::<u128>()` is 8, or Rust 1.77 and
newer, where it is 16 on x86_64. Compile-time guards enforce the invariant:
`const _: () = assert!(size_of::<T>() == N)` for sizes in `perp_market.rs` and
`spot_market.rs`, and `static_assertions::const_assert_eq!` for field offsets in `state.rs`.

- Padding bytes are not uninitialized memory bugs.
- Field order is load-bearing for onchain ABI compatibility, so reordering is not a
  cosmetic refactor.
- Adding a field has to preserve the invariant. A PR that adds a `u128` or `i128` field
  after a `PoolBalance` field is worth flagging.

### 2. Custom native entrypoint

Velocity dispatches three high-frequency oracle instructions through a custom native
entrypoint that bypasses Anchor's instruction discriminator and account deserialization.
The leading bytes `[0xFF, 0xFF, 0xFF, 0xFF, <opcode>]` signal that path, and `lib.rs` wires
opcode 0 to `update_mm_oracle_native`, opcode 1 to `update_amm_spread_adjustment_native`,
and opcode 2 to `update_mm_oracle_batch_native`. Every other instruction falls through to
the Anchor entrypoint. Manual account deserialization inside these three handlers is
expected.

- This is not unsafe deserialization of untrusted input. The dispatch is deliberate and
  each handler validates its own accounts.
- Do not flag the absence of `#[derive(Accounts)]` on this path.

### 3. `remaining_accounts` for variable-length account lists

Many instructions take a variable number of oracle accounts, spot markets or maker accounts
through Anchor's `remaining_accounts`. The `Accounts` derive does not validate these.
Validation lives in `programs/velocity/src/validation/` and in the per-instruction logic
instead.

- Unvalidated `remaining_accounts` is a finding only when the instruction never reaches the
  validation layer. Check `programs/velocity/src/validation/` and the instruction's
  controller before reporting it.

### 4. `AccountLoader` zero-copy on large accounts

`User` and `PerpMarket` load through Anchor's `AccountLoader`, which returns `Ref` and
`RefMut` views into the underlying buffer without copying. The patterns around it look
unsafe but are correct given the alignment invariant in section 1.

- Do not flag `load()` or `load_mut()` on their own.
- Do flag a new zero-copy struct that breaks the alignment invariant, or a field access
  outside the `RefMut` lifetime.

## Generated and non-source artifacts, skip review

Diffs in these paths carry no security signal beyond their upstream sources, so skip them:

- `packages/sdk/src/idl/velocity.json` and `packages/sdk/src/idl/velocity.ts`, regenerated
  from the Anchor program. Any meaningful change already shows up in the Rust diff.
- `rust/velocity-rs/crates/src/velocity_idl.rs`, generated from that same IDL.
- Markdown changes (`**/*.md`), which carry no executable behavior.
- The `bun.lock` and `Cargo.lock` lockfiles. Snyk SCA covers dependency hygiene, not this
  review.

## What is in scope

Report on:

- New unsafe blocks or pointer arithmetic outside the zero-copy pattern above.
- Instructions that read user-controlled accounts without calling the validation layer.
- Oracle staleness, price manipulation, and rounding-direction bugs that affect margin or
  settlement math.
- Missing authority or signer checks on admin and keeper instructions.
- Integer overflow and underflow in pricing, funding or PnL math
  (`programs/velocity/src/math/`).
- SDK code that builds transactions from attacker-controllable fields without bounds
  checks.
- Secret material, API keys or RPC endpoints committed by accident.

When a finding is uncertain, report it and say why it might still be fine given the
patterns above, so reviewers can triage it quickly.
