# Security Review — Isolated Perp Position & VLP Hedge Gates

- **Date:** 2026-09-25
- **Scope:** `programs/velocity/src/controller/isolated_position.rs`,
  `programs/velocity/src/vlp/hedge/` (`instructions.rs`, `state.rs`,
  `settle.rs`, `math.rs`, `admin.rs`, `constituent_map.rs`, `mod.rs`),
  gated entry points in `programs/velocity/src/lib.rs`, and the
  corresponding `#[derive(Accounts)]` structs.
  These two feature gates (`isolated-position`, `vlp-hedge`) are compiled
  out of mainnet builds pending audit.
- **Method:** manual code review against fund-flow conservation, collateral
  isolation, authentication/authorization, oracle-validity gating, account
  layout stability, and integer-safety invariants, cross-checked against the
  equivalent ungated code paths for consistency.

Findings F1–F3 below were verified in code and are accompanied by minimal
patches. Notes N1–N4 are open hardening items recorded without code changes.

---

## F1 — Hedge keeper cranks lack hot-wallet role binding (Medium, fixed)

**Location:** `programs/velocity/src/vlp/hedge/instructions.rs` —
`UpdateConstituentOracleInfo`, `UpdateConstituentTargetBase`, `UpdateLPPoolAum`

The three keeper-crank structs that write LP-pool state (`last_oracle_price`,
target weights, AUM inputs) required only a bare `keeper: Signer`, while
sibling fund-moving instructions in the same module enforce
`check_hot(..., HotRole::LpSwap)`, and the protocol's own oracle-writer crank
(`update_mm_oracle`) enforces a hot-wallet check. Any holder of a keeper key
— regardless of role — could overwrite oracle cache data feeding AUM and swap
pricing.

**Fix applied:** added
`constraint = check_hot(&keeper.key(), &state, HotRole::LpCache)?`
to the `keeper` field of all three structs. No handler logic changed; existing
callers signing with a warm/cold admin key are unaffected (`is_hot` admits
warm signers).

## F2a — Flash-swap path bypasses pause gates (Medium, fixed)

**Location:** `programs/velocity/src/vlp/hedge/admin.rs` —
`handle_begin_lp_swap`

Unlike the normal `handle_lp_pool_swap`, the flash-swap entry point performed
no exchange-pause check, no `allow_swap_lp_pool` kill-switch check, and no
per-constituent operation check, so a hot signer could flash-borrow while
swaps were paused or a constituent was gated.

**Fix applied:** added `#[access_control(fill_not_paused(...))]`,
an `allow_swap_lp_pool()` validation, and
`does_constituent_allow_operation(Swap)` on both constituents before any
state mutation or transfer, mirroring the normal swap path. No AUM-freshness
gate was added: this path consumes no AUM or pricing math.

## F2b — Constituent decimals accepted unchecked at init (Low, fixed)

**Location:** `programs/velocity/src/vlp/hedge/admin.rs` —
`handle_initialize_constituent`

The caller-supplied `decimals` was stored without comparison against the
bound spot market's decimals, while notional/target math scales by
`10^constituent.decimals` and balances use spot precision. A mismatch
silently misprices the constituent.

**Fix applied:** added
`validate!(spot_market.decimals == decimals, InvalidConstituent)`
before any mutation. The field is written only at init (no update path), and
the spot market account was already in context, so the check is a one-liner
with no interface change.

## F3a — Isolated deposit/withdraw missing pool-id leg (Medium, fixed)

**Location:** `programs/velocity/src/controller/isolated_position.rs` —
`deposit_into_isolated_perp_position`, `withdraw_from_isolated_perp_position`

The transfer path enforces a three-way check
(`user.pool_id == spot_market.pool_id && user.pool_id == perp_market.pool_id`),
but deposit checked only the user↔spot leg and withdraw checked none,
allowing collateral to attach to an isolated position across pool boundaries.

**Fix applied:** mirrored the three-way pool check into both functions.

## F3b — Isolated deposit path ignores market Deposit pause (Medium, fixed)

**Location:** `programs/velocity/src/controller/isolated_position.rs` —
`deposit_into_isolated_perp_position`, `transfer_isolated_perp_position_deposit`

The ungated deposit path rejects `SpotOperation::Deposit`-paused markets, but
the isolated deposit (and the deposit direction of transfer) did not, so
collateral could enter a paused market through the gated feature.

**Fix applied:** added the `SpotOperation::Deposit` pause validation
(mirroring the ungated path, same error code) to the deposit function and to
the `amount > 0` branch of transfer. The exit direction (`amount < 0`) and
the dedicated withdraw path intentionally remain pause-free for outflows, so
collateral can always leave.

---

## Open notes (no code change)

- **N1 — AUM sums unsettled debt across all pools (Medium, conditional).**
  `update_aum` aggregates `quote_owed_from_lp_pool` over every AMM-cache
  entry without filtering on `hedge_config.pool_id == lp_pool.lp_pool_id`.
  Harmless in single-pool deployments; must be fixed (filter + fail closed on
  missing markets) before enabling multi-pool.
- **N2 — Flash path skips reduce-only checks (Medium).** `begin_lp_swap`
  does not apply directional reduce-only checks to the in/out constituents.
  Adding them requires loading both spot markets into the instruction
  context (interface change affecting the SDK and tests), so it is left for
  the gate-opening change set.
- **N3 — Flash-loan repayment is snapshot-based (Medium, by design).**
  `end_lp_swap` sweeps only surplus over the pre-loan snapshot and enforces
  no principal-repayment invariant; safety rests on same-transaction
  begin/end introspection, the middle-instruction whitelist, and the
  authorized hot taker. Any relaxation of those assumptions should revisit
  this path.
- **N4 — Oracle-validity hardening on deposit paths (Info, protocol-wide).**
  Vault/isolated deposit handlers use raw oracle prices for interest accrual
  and dust-check accounting, identically to the long-standing ungated deposit
  path. Transferred amounts are caller-specified and hard-bounded, and AUM
  valuation re-gates on fresh oracles, so no fund-loss hole was found; a
  uniform validity-gate policy would need to cover gated and ungated paths
  together to avoid divergence.

---

## Non-issues confirmed during review (for completeness)

- Isolated↔cross transfer direction semantics (`Borrow` leg reducing an
  existing deposit first) are correct by construction of the shared balance
  helper, with a post-transfer margin gate.
- `can_sign_for_user` on the isolated structs is strictly stronger than
  `has_one = authority` (supports delegates).
- The `forced velocity_signer` + runtime key-equality pattern and the
  always-compiled hedge state modules are intentional, protocol-wide, and
  layout-stable across build flavors.
- The swap-struct auxiliary accounts are bound both ways (stored pool id and
  pool-stored PDA address), which is sufficient without extra seed
  constraints.
