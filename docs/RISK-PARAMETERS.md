# Risk parameters

Every admin-settable parameter of the Velocity program: which instruction sets it, who can call
that instruction, what units the value is in, what range the program enforces on it, and what a
change does to a user with an open position, a resting order, or a pending withdrawal.

Two facts frame everything below:

1. **There is no on-chain timelock.** The program checks the signer against the admin keys stored
   on `State` and applies the write in the same transaction (`auth.rs:1-14` states the cold admin
   is *expected* to be an off-chain multisig plus timelock; the pause admin deliberately has
   none). Any delay between a decision and its effect lives in off-chain key custody, not in the
   program.
2. **Nothing grandfathers open positions.** Almost every parameter is read live from the account
   it lives on, so a change takes effect at the next instruction that reads it. A tightened
   maintenance margin ratio can make an already-open position liquidatable at the next keeper
   evaluation. The per-parameter tables below say when each value actually bites; the few latched
   exceptions (TWAP-fed inputs, per-period counters, the equity-floor breaker) are called out
   where they occur.

This is a reference for operators changing parameters and for integrators predicting what a
change does. It describes what the program enforces today; it does not restate the fee system or
the equity floor, which have their own documents (see [Related documents](#related-documents)).

## How to read this document

**File paths.** Bare paths like `validation/margin.rs:22` are relative to
`programs/velocity/src/`. Four files are cited so often they get shorthand names:
`admin.rs` = `instructions/admin.rs`, `keeper.rs` = `instructions/keeper.rs`,
`constraints.rs` = `instructions/constraints.rs`, and `constants.rs` = `math/constants.rs`
(the `vlp/hedge/admin.rs` twin is always written in full). Paths outside the program (SDK,
docs) are given from the repo root. Line numbers are exact as of the commit that introduced
this document; they drift as files are edited, so treat them as anchors for `git log -L`, not
eternal truths.

**Authority tiers.** Three additive signer tiers live on `State` (`auth.rs:1-14`):

| Tier | Meaning | Constraint helper |
| --- | --- | --- |
| cold | Root authority only (`state.cold_admin`). Reserved for actions that can undermine other safety rails. | `check_cold` (`auth.rs:64`) |
| warm | Cold **or** warm (`state.warm_admin`, the operational multisig). Most parameter setters sit here. | `check_warm` (`auth.rs:72`) |
| hot | Cold, warm, **or** the purpose-specific bot key for one `HotRole`. | `check_hot` (`auth.rs:79`) |
| pause | Cold, warm, **or** the dedicated emergency `state.pause_admin`. | `check_pause` (`auth.rs:86`) |

The tier column below names the *lowest* tier that can call the instruction; every tier above it
can too. For pause-reachable bitmask setters, `require_pause_only_added` (`auth.rs:112`) lets the
pause admin only *add* pause bits; clearing any bit needs warm or cold.

**Units.** Values are fixed-point integers scaled by the constants in `math/constants.rs`:

| Constant | Value | 1 unit means |
| --- | --- | --- |
| `MARGIN_PRECISION` | 1e4 (`constants.rs:38`) | 0.01% of a margin ratio; 10000 = 1x leverage |
| `SPOT_WEIGHT_PRECISION` | 1e4 (`constants.rs:41`) | 0.01% of an asset/liability weight |
| `PERCENTAGE_PRECISION` | 1e6 (`constants.rs:52`) | 0.0001%; 1_000_000 = 100% |
| `LIQUIDATION_FEE_PRECISION` | 1e6 (`constants.rs:75`) | 0.0001% of notional |
| `SPOT_IMF_PRECISION` | 1e6 (`constants.rs:77`) | IMF size-premium scale |
| `QUOTE_PRECISION` | 1e6 (`constants.rs:30`) | 1 micro-USD (1_000_000 = $1) |
| `PRICE_PRECISION` | 1e6 (`constants.rs:22`) | price tick of the oracle scale |
| `BASE_PRECISION` | 1e9 (`constants.rs:16`) | 1e-9 of one base unit |
| `FUNDING_RATE_PRECISION` | 1e9 (`constants.rs:102`) | funding-rate scale |

**"unchecked".** A bound column that says **unchecked** means the handler writes the value with
no validation at all, on-chain, after following every helper the handler calls. It is a verified
claim about the program, not an observation that a check was hard to find; each one survived a
second adversarial pass that actively searched for a validator. It says nothing about off-chain
guards (the admin CLI, multisig policy, or deploy scripts may well constrain the same value), and
where a type's own width is the only limit, the tables say so. The confirmed cases where an
unchecked value can damage users are collected in
[Missing bounds worth knowing about](#missing-bounds-worth-knowing-about).

**What "takes effect" means.** Solana programs only run inside transactions. "Immediately at the
next X" below means: the moment any transaction executes instruction X after the parameter write
lands, the new value governs that execution. There is no epoch boundary, no cache, and no
per-user snapshot unless a row says otherwise.

## Margin and liquidation

The best-guarded family in the program. Perp margin ratios and liquidation fees flow through the
shared validator `validate_margin` (`validation/margin.rs:15-53`, unit-tested at
`validation/margin.rs:141-173`), reached from the setters via
`AMM::validate_compatible_with_margin_ratio` (`vlp/amm/state.rs:481-495`) and
`validate_compatible_with_liquidation_fee` (`vlp/amm/state.rs:501-515`). The `State`-level
liquidation tunables, by contrast, are all bare assignments.

| Field | Set by | Tier | Units | Enforced bounds |
| --- | --- | --- | --- | --- |
| `PerpMarket.margin_ratio_initial` | `update_perp_market_margin_ratio` (`admin.rs:1441`) | warm | `MARGIN_PRECISION` (1e4) | 125..=10000 (`MIN`/`MAX_MARGIN_RATIO`, `constants.rs:213-214`); strictly > maintenance; `initial * 100 > amm.max_spread` (`validation/margin.rs:22-24,30-32,44-50`) |
| `PerpMarket.margin_ratio_maintenance` | same handler | warm | `MARGIN_PRECISION` | 125..=10000; strictly < initial; `maintenance * 100 > liquidator_fee + if_liquidation_fee` (`validation/margin.rs:26-32,34-42`) |
| `PerpMarket.liquidator_fee` | `update_perp_liquidation_fee` (`admin.rs:1648`) | warm | `LIQUIDATION_FEE_PRECISION` (1e6) | fee sum (all three) strictly < 100% (`admin.rs:1661-1668`); cross-checked against maintenance margin (`validation/margin.rs:34-42`) |
| `PerpMarket.if_liquidation_fee` | same handler | warm | `LIQUIDATION_FEE_PRECISION` | < 100% standalone (`admin.rs:1670-1674`); in the sum and the margin cross-check |
| `PerpMarket.protocol_liquidation_fee` | same handler | warm | `LIQUIDATION_FEE_PRECISION` | <= 10% (`admin.rs:1676-1680`); in the sum; *not* in the margin cross-check |
| `SpotMarket.liquidator_fee` | `update_spot_market_liquidation_fee` (`admin.rs:1755`) | warm | `LIQUIDATION_FEE_PRECISION` | fee sum strictly < 100% (`admin.rs:1767-1774`) and nothing else; see [missing bounds](#missing-bounds-worth-knowing-about) |
| `SpotMarket.if_liquidation_fee` | same handler | warm | `LIQUIDATION_FEE_PRECISION` | <= 10% (`admin.rs:1776-1780`); in the sum |
| `SpotMarket.protocol_liquidation_fee` | same handler | warm | `LIQUIDATION_FEE_PRECISION` | <= 10% (`admin.rs:1782-1786`); in the sum |
| `PerpMarket.imf_factor` | `update_perp_market_imf_factor` (`admin.rs:2403`) | warm | `SPOT_IMF_PRECISION` (1e6); the field doc comment says `MARGIN_PRECISION` and is wrong (see below) | <= 1_000_000 (`admin.rs:2408-2412`) |
| `PerpMarket.unrealized_pnl_imf_factor` | same handler | warm | `SPOT_IMF_PRECISION`; same wrong doc comment | <= 1_000_000 (`admin.rs:2413-2417`) |
| `PerpMarket.unrealized_pnl_initial_asset_weight` | `update_perp_market_unrealized_asset_weight` (`admin.rs:2441`) | warm | `SPOT_WEIGHT_PRECISION` (1e4) | <= 10000 and <= the maintenance weight (`admin.rs:2446-2450,2456-2460`) |
| `PerpMarket.unrealized_pnl_maintenance_asset_weight` | same handler | warm | `SPOT_WEIGHT_PRECISION` | <= 10000 and >= the initial weight (`admin.rs:2451-2460`) |
| `PerpMarket.unrealized_pnl_max_imbalance` | `update_perp_market_max_imbalances` (`admin.rs:1546`) | warm | `QUOTE_PRECISION` | <= tier insurance cap + 1 (`admin.rs:1568-1576`; tier caps `constants.rs:147-150`) |
| `PerpMarket.insurance_claim.quote_max_insurance` | same handler | warm | `QUOTE_PRECISION` | <= tier insurance cap; >= insurance already settled (`admin.rs:1568-1583`) |
| `PerpMarket.insurance_claim.max_revenue_withdraw_per_period` | same handler | warm | `QUOTE_PRECISION` | <= max(tier cap, 250 USD); >= the current period's draw (`admin.rs:1568-1576,1608`) |
| `PerpMarket.contract_tier` | `update_perp_market_contract_tier` (`admin.rs:2382`) | warm | enum `ContractTier` (`state/perp_market.rs:80-94`) | **unchecked** |
| `State.initial_pct_to_liquidate` | `update_initial_pct_to_liquidate` (`admin.rs:2541`) | warm | `LIQUIDATION_PCT_PRECISION` (1e4, `constants.rs:46`) | **unchecked** (values above 10000 are inert: the sum is min'd at `math/liquidation.rs:440`) |
| `State.liquidation_duration` | `update_liquidation_duration` (`admin.rs:2555`) | warm | slots, u8 | **unchecked** (0 is safe by fallback: `math/liquidation.rs:437-438` defaults to 100% freeable) |
| `State.liquidation_margin_buffer_ratio` | `update_liquidation_margin_buffer_ratio` (`admin.rs:2569`) | warm | `MARGIN_PRECISION`; default 2% (`constants.rs:249`) | **unchecked** |
| `User.equity_floor` | `update_user_equity_floor` (`admin.rs:4440`) | warm | `QUOTE_PRECISION`; 0 disables | **unchecked** |
| `User.equity_floor_buffer` | same handler | warm | `QUOTE_PRECISION`; inert while floor is 0 | **unchecked** (u128 saturating add, cannot overflow: `state/user.rs:209`) |

### What a change does

**Margin ratios** (`update_perp_market_margin_ratio`). The two ratios feed
`PerpMarket::get_margin_ratio` (`state/perp_market.rs:821-851`). The initial ratio governs order
placement and withdrawals (via `math/margin.rs:157`) and half of the Fill-type average; raising
it lowers every position holder's free collateral at the next margin computation, but it never
decides liquidation eligibility. The maintenance ratio is what liquidation entry compares against
(`controller/liquidation.rs:203-217` against the unbuffered requirement,
`state/margin_calculation.rs:416-418`): raising it can make an already-open position liquidatable
at the next keeper evaluation, with no grace period. Both bite immediately; nothing is latched.

**Liquidation fees.** Perp fees are read on every perp liquidation fill
(`controller/liquidation.rs:403,423-427,530,536`); the effective liquidator fee ages upward per
slot after a grace period and is clamped to
`min(liquidator_fee * 3, margin_ratio_maintenance * 100)` (`state/perp_market.rs:857-864`,
`math/liquidation.rs:587-605`). Spot fees are read raw on spot liquidation paths
(`controller/liquidation.rs:1479,1551,1677-1678,2154-2155,2531-2532`), with no aging and no
maintenance-weight clamp. A user liquidated on the fill after the change pays the new rate. Fee
routing is described in [FEES.md](../FEES.md).

**IMF factors and unrealized-PnL weights.** `imf_factor` adds a size premium to *all three*
margin requirement types for large positions (`state/perp_market.rs:842`,
`math/margin.rs:51-84`; the premium can only raise the requirement), so raising it can push a
large open position toward liquidation. The three `unrealized_pnl_*` weight/factor/imbalance
fields only shape the Initial and Fill branches of `get_unrealized_asset_weight`
(`state/perp_market.rs:868-922`), with one exception: `unrealized_pnl_maintenance_asset_weight`
is returned untouched on the Maintenance branch (`state/perp_market.rs:922`), so lowering *that
one* field reduces the collateral credit of unrealized profits in liquidation-eligibility checks
and can newly flag open winners. The `unrealized_pnl_max_imbalance` discount compares against a
net-unsettled-PnL figure computed from the stored `historical_oracle_data.last_oracle_price`
(`state/perp_market.rs:887`), so the size of that discount moves with the oracle crank, not the
live price.

**Insurance claim caps** (`update_perp_market_max_imbalances`). These do not touch positions.
`quote_max_insurance` caps cumulative insurance spend for the market's bankruptcy and PnL-deficit
repair (`controller/insurance.rs:842-891`, `controller/liquidation.rs:4101-4110`);
`max_revenue_withdraw_per_period` caps the per-period draw out of the insurance fund into the
market's PnL pool (`controller/insurance.rs:824-840`). Lowering either shrinks the next solvency
repair; at 0 the insurance tranche is skipped and losses fall through to the socialized-loss
path.

**Contract tier** (`update_perp_market_contract_tier`, **unchecked**). The tier switches
liquidation ordering (positions in less-safe markets must close first,
`controller/liquidation.rs:3448-3525`), oracle-divergence and volatility tolerances
(`state/perp_market.rs:1028,1053,1208-1210`), and the insurance caps enforced by the *max
imbalances* setter (`admin.rs:1556-1566`). Two hazards: the change re-prioritizes liquidation
ordering for every open position immediately, and a downgrade silently leaves the previously-set
insurance caps above the new tier's limit (the speculative tiers cap insurance at 0,
`constants.rs:150`) until an admin separately re-runs the max-imbalances setter. Nothing
re-validates the pair.

**State-level liquidation tunables** (all **unchecked**, all read live from `State` on every
liquidation). `initial_pct_to_liquidate` is the baseline share of the margin shortage a
liquidator may free at once, and `liquidation_duration` is the slot ramp toward 100%
(`math/liquidation.rs:420-451`); shortening the duration or raising the pct makes every
liquidation, in-progress ones included, seize more per fill. `liquidation_margin_buffer_ratio`
does not decide who is liquidatable (entry is unbuffered); it sets how deep a liquidation goes
before it can exit (`state/margin_calculation.rs:240-246,446-453`) and when a flagged user is
released to place orders again (`math/liquidation.rs:258-287`). As an uncapped u32 it can be set
high enough that no liquidation can ever exit, permanently freezing flagged users out of new
orders; large values fail closed via safe-math rather than misbehaving.

**Equity floor** (`update_user_equity_floor`, per-`User`, both fields **unchecked**). Raising a
subaccount's floor above its current net equity makes the authority freezable: the authority-wide
breaker is latched, so the freeze lands at the next permissionless trip
(`instructions/keeper.rs:306-364`) or lazy trip inside a fill or force-cancel
(`controller/equity_floor.rs:34-60`), while the per-subaccount effects (risk-increasing fill
cancellation, delegate-transfer gates) bite at the next such instruction. Raising the buffer can
make a tripped breaker unresettable until equity recovers, because the warm-admin reset requires
every subaccount to clear floor + buffer (`admin.rs:4419-4427`). The full lifecycle is in
[docs/EQUITY-FLOOR.md](./EQUITY-FLOOR.md).

**A doc-comment trap for operators:** the field docs of `PerpMarket.imf_factor` and
`unrealized_pnl_imf_factor` (`state/perp_market.rs:380-385`) claim `MARGIN_PRECISION` (1e4). The
enforced bound and the formula denominator are both `SPOT_IMF_PRECISION` (1e6). A value chosen on
the 1e4 assumption is 100x too small. The in-repo test spells out the real scale
(`math/margin/tests.rs:263`).

## Spot markets: weights, rates, and caps

Spot margin weights flow through `validate_margin_weights` (`validation/margin.rs:55-135`),
which pins the quote market (index 0) to exactly 10000 on all four weights and enforces the
initial/maintenance orderings elsewhere. The borrow-rate triple flows through
`validate_borrow_rate` (`validation/spot_market.rs:24-38`). The deposit-side rate limiter and a
few one-field setters are bare assignments.

| Field | Set by | Tier | Units | Enforced bounds |
| --- | --- | --- | --- | --- |
| `SpotMarket.initial_asset_weight` | `update_spot_market_margin_weights` (`admin.rs:2009`) | warm | `SPOT_WEIGHT_PRECISION` (1e4) | market 0: exactly 10000; others: <= 10000 and <= maintenance weight; 0 allowed (`validation/margin.rs:64-69,92-106`) |
| `SpotMarket.maintenance_asset_weight` | same handler | warm | `SPOT_WEIGHT_PRECISION` | market 0: exactly 10000; others: > 0, <= 10000, >= initial weight (`validation/margin.rs:71-76,99-106`) |
| `SpotMarket.initial_liability_weight` | same handler | warm | `SPOT_WEIGHT_PRECISION` | market 0: exactly 10000; others: >= 10000 and >= maintenance liability weight; no upper cap (`validation/margin.rs:78-83,108-121`) |
| `SpotMarket.maintenance_liability_weight` | same handler | warm | `SPOT_WEIGHT_PRECISION` | market 0: exactly 10000; others: >= 10000, <= initial liability weight (`validation/margin.rs:85-90,115-121`) |
| `SpotMarket.imf_factor` | same handler | warm | `SPOT_IMF_PRECISION` (1e6); the field doc comment says `MARGIN_PRECISION` and is wrong | 0 <= value < 1_000_000, strict (`validation/margin.rs:124-130`) |
| `SpotMarket.optimal_utilization` | `update_spot_market_borrow_rate` (`admin.rs:2071`) | warm | `SPOT_UTILIZATION_PRECISION` (1e6) | <= 1_000_000 (`validation/spot_market.rs:17-22`) |
| `SpotMarket.optimal_borrow_rate` | same handler | warm | `SPOT_RATE_PRECISION` (1e6) | >= expanded min rate, <= max rate (`validation/spot_market.rs:24-38`) |
| `SpotMarket.max_borrow_rate` | same handler | warm | `SPOT_RATE_PRECISION` | >= optimal rate only; no absolute ceiling (u32 domain is ~4295x = ~429,496% APR) (`validation/spot_market.rs:24-30`) |
| `SpotMarket.min_borrow_rate` | same handler | warm | u8, 1 unit = 0.5% (`state/spot_market.rs:197-200`) | expanded (u8 x 5000) <= optimal rate; re-validated even when unchanged (`admin.rs:2086`, `validation/spot_market.rs:32-38`) |
| `SpotMarket.withdraw_guard_threshold` | `update_withdraw_guard_threshold` (`admin.rs:1815`) | warm | token mint precision | oracle live price and 5-min TWAP both > 0; notional at max(live, TWAP) <= $10,000 (`admin.rs:1835-1848`, `validation/spot_market.rs:46-74`, cap `constants.rs:265`) |
| `SpotMarket.withdraw_circuit_breaker_bps` | `update_spot_market_withdraw_circuit_breaker` (`admin.rs:2148`) | warm; **cold to exceed 25%** | bps (10000 = 100%) | <= 10000 for anyone; <= 2500 unless the signer is cold (`admin.rs:2152-2172`) |
| `SpotMarket.max_token_deposits` | `update_spot_market_max_token_deposits` (`admin.rs:2128`) | warm | token mint precision; 0 = no limit | **unchecked** |
| `SpotMarket.max_token_borrows_fraction` | `update_spot_market_max_token_borrows` (`admin.rs:2217`) | warm | fraction of `max_token_deposits`, x/10000 | derived cap must cover current outstanding borrows (`admin.rs:2230-2242`); the fraction itself may exceed 100% |
| `SpotMarket.deposit_guard_threshold` | `update_spot_market_deposit_cap` (`admin.rs:2190`) | warm | token mint precision | **unchecked** |
| `SpotMarket.max_deposit_bps_per_day` | same handler | warm | bps/day; 0 disables the limiter | **unchecked** (u16 caps it at 655%/day) |
| `SpotMarket.scale_initial_asset_weight_start` | `update_spot_market_scale_initial_asset_weight_start` (`admin.rs:2251`) | warm | `QUOTE_PRECISION` notional; 0 disables | **unchecked** |
| `SpotMarket.insurance_fund.unstaking_period` | `update_insurance_fund_unstaking_period` (`admin.rs:1735`) | warm | seconds (i64); default 13 days (`constants.rs:124`) | **unchecked** |
| `SpotMarket.insurance_fund.revenue_settle_period` | `update_spot_market_revenue_settle_period` (`admin.rs:1914`) | warm | seconds (i64) | > 0 only (`admin.rs:1921`) |
| `SpotMarket.insurance_fund.if_fee_factor` | `update_spot_market_if_factor` (`admin.rs:1865`) | warm | `IF_FACTOR_PRECISION` (1e6) | `if_fee_factor + protocol_fee_factor` strictly < 100% (`admin.rs:1887-1891`) |
| `SpotMarket.protocol_fee_factor` | same handler | warm | `IF_FACTOR_PRECISION` | same joint bound; no individual cap |
| `SpotMarket.if_paused_operations` | `update_spot_market_if_paused_operations` (`admin.rs:2291`) | pause | u8 bitmask of `InsuranceFundOperation` (`state/paused_operations.rs:88-94`) | pause admin may only add bits (`admin.rs:2298-2303`, `auth.rs:112-122`); warm/cold set anything |

### What a change does

**Margin weights.** All four weights and the spot `imf_factor` are read live inside
`get_asset_weight` / `get_liability_weight` (`state/spot_market.rs:425-518`). Lowering an asset
weight or raising a liability weight immediately reduces free collateral for every holder at the
next margin computation; the maintenance variants move users toward their liquidation boundary
the same way the perp maintenance ratio does. One coupling is enforced only one way:
`update_spot_market_asset_tier` requires the tier to be Collateral or Protected when
`initial_asset_weight > 0` (`admin.rs:1981-2004`), but this weights handler can raise the weight
above 0 on a Cross/Isolated/Unlisted-tier market without any check firing.

**Borrow-rate curve** (`update_spot_market_borrow_rate`). The triple shapes
`calculate_borrow_rate` (`math/spot_balance.rs:225-271`): the optimal point is the pivot between
the linear low-utilization slope and the hard-coded high-utilization segments
(`constants.rs:271-278`), `max_borrow_rate` is the rate at 100% utilization, and the expanded
`min_borrow_rate` floors the result at any utilization. `optimal_utilization` also feeds the
withdraw/borrow utilization ceiling (`math/spot_withdraw.rs:236`); at the permitted extreme of
100% it effectively disables that half of the limiter. The timing here is the sharpest edge in
this family: interest accrual is computed over the whole interval since `last_interest_ts` in one
shot (`math/spot_balance.rs:157-181`), so a rate change is applied **retroactively to all
un-accrued elapsed time**, and there is no absolute ceiling on `max_borrow_rate`. One warm-admin
write followed by any instruction that touches the market can bill every existing borrower at an
extreme rate for the elapsed span.

**Withdraw guard and circuit breaker.** `withdraw_guard_threshold` is the balance floor below
which the daily withdraw/borrow limiter never restricts, and the budget for the small-depositor
bypass (`math/spot_withdraw.rs:213,248-256,296-308,378`). It is the only field in the subsystem
with an absolute enforced notional cap ($10,000 at the stricter of live price and 5-min TWAP),
and the cold-only oracle setter exists precisely because swapping the oracle would re-price that
cap (`admin.rs:4728-4739`). `withdraw_circuit_breaker_bps` sets how much of the 24h deposit TWAP
may leave per day (`math/spot_withdraw.rs:21-47`); 0 is a sentinel for the default 25%, not a
freeze, and only the cold admin can loosen past 25%. Lowering it can block a withdrawal that
would have succeeded a moment earlier.

**Deposit and borrow caps.** `max_token_deposits` gates new deposits post-credit
(`state/spot_market.rs:547-581`); lowering it below current deposits blocks new deposits but
unwinds nothing. Setting it to 0 disables the deposit cap *and* the borrow cap in the runtime
check (both short-circuit on it, `state/spot_market.rs:555,562`), a stronger effect than the
field doc's "no limit" suggests. `max_token_borrows_fraction` is validated against outstanding
borrows at set time, which is stricter than the runtime check in two awkward ways: with
`max_token_deposits == 0` the setter rejects *any* fraction while borrows are outstanding, and
even setting 0 to disable is rejected then, so the setter can lock itself until borrows fall.
The deposit-side rate limiter (`deposit_guard_threshold`, `max_deposit_bps_per_day`) is entirely
unchecked, ships disabled by default (0 = no daily cap), and its threshold has no equivalent of
the $10k notional cap its withdraw-side mirror carries.

**Scaled initial asset weight** (`scale_initial_asset_weight_start`, **unchecked**). Once the
market's total deposit notional exceeds this value, every depositor's initial asset weight is
scaled down by `start / deposit_value` (`state/spot_market.rs:460-482`). The hazardous direction
is small-but-nonzero: a value of 1 (one millionth of a dollar) collapses the market's collateral
contribution toward 0 protocol-wide in one write. 0 disables the scaling, and no check
distinguishes the two intents.

**Insurance fund settings.** `unstaking_period` is read at unstake *completion* time, not
latched at request time (`controller/insurance.rs:443-446`), so a change re-times every pending
unstake: zero or negative values (it is a signed i64, fully unchecked) instantly release all
pending unstakes, and a huge value strands them. `revenue_settle_period` gates and sizes the
per-period revenue settle into the insurance fund (`controller/insurance.rs:608-663`); it also
divides into the per-settle APR cap, so shortening it settles more often in smaller slices. The
two fee factors carve the IF and protocol shares off depositors' accrued interest
(`controller/spot_balance.rs:207-236`), jointly bounded below 100% because a full carveout would
freeze the whole accrual block (the comment at `admin.rs:1881-1886` spells this out); the same
retroactive-interval timing as the rate curve applies. `if_paused_operations` blocks individual
IF-stake operations per bit; pausing `Remove` strands stakers who already served their unstaking
period until the bit is cleared (warm or cold only).

## Oracles and staleness

Oracle *identity* changes are the only setters in the program reserved for the cold admin
(`auth.rs:60-63` explains why: an oracle swap re-prices the withdraw-guard notional cap and every
other rail). The staleness and validity thresholds, by contrast, all live in one `State` struct
written by a single bare assignment.

| Field | Set by | Tier | Units | Enforced bounds |
| --- | --- | --- | --- | --- |
| `SpotMarket.oracle` + `oracle_source` | `update_spot_market_oracle` (`admin.rs:1013`) | **cold** | Pubkey + `OracleSource` enum | account must decode under the source; new price > 0 and within 10% of the old (`admin.rs:1023-1093`), unless `skip_invariant_check=true`; four Pyth-pull variants rejected (`admin.rs:100-112`); see caveats below |
| `PerpMarket.oracle` + `oracle_source` | `update_perp_market_oracle` (`admin.rs:2645`) | **cold** | same | same checks (`admin.rs:2657-2728`), same caveats |
| `State.oracle_guard_rails.*` (all 6 fields) | `update_oracle_guard_rails` (`admin.rs:2586`) | warm | see below | **unchecked**: one bare struct assignment (`admin.rs:2596`), no validator anywhere |
| `PerpMarket.oracle_low_risk_slot_delay_override` | `update_perp_market_oracle_low_risk_slot_delay_override` (`admin.rs:3041`) | warm | slots (i8); 0 = fall back to state | **unchecked** |
| `PerpMarket.oracle_slot_delay_override` | `update_perp_market_oracle_slot_delay_override` (`admin.rs:3061`) | warm | slots (i8); negative = unset; 0 disables immediate AMM fills | **unchecked** |
| `PrelaunchOracle.price` / `max_price` | `update_prelaunch_oracle_params` (`admin.rs:3255`) | warm | `PRICE_PRECISION` (1e6) | both != 0 and `price <= max_price` (`state/oracle.rs:758-772`); no sign or magnitude bound |
| `State.feature_bit_flags` MmOracleUpdate bit | `update_feature_bit_flags_mm_oracle` (`admin.rs:4126`) | hot (`HotRole::FeatureFlag`) to disable; **cold to re-enable** (`admin.rs:4132-4136`) | one bit | single-bit read-modify-write; other bits undisturbed |

### Oracle swaps (cold)

Both swap handlers enforce price continuity (new price > 0, within 10% of the old,
`constants.rs:55`) unless the admin passes `skip_invariant_check=true`, which reduces the check
to "the account decodes". Two escape hatches survive even with the invariant check on:

- `Pubkey::default()` is accepted outright by `validate_oracle_account_info`
  (`state/oracle_map.rs:348-358`) and then reads as a permanently **valid $1.00 price with zero
  delay** (`state/oracle_map.rs:71-73,109-111`), bypassing every staleness, confidence, and
  volatility gate for that market.
- `OracleSource::QuoteAsset` returns a synthetic $1 without reading the account at all
  (`state/oracle.rs:456-490`), so the decodability requirement is vacuous under it.

A swap re-prices margin, unrealized PnL, funding, and liquidation eligibility for every open
position at the next instruction that reads the market. Choosing a `*StableCoin` source variant
also changes behavior silently: it triples the margin-staleness budget
(`math/oracle.rs:423-429`) and snaps prices within 5 bps to exactly $1
(`state/oracle.rs:596-606`). Switching a perp market to `OracleSource::Prelaunch` hands price
control to the admin-writable `PrelaunchOracle` account.

### Oracle guard rails (warm, all six fields unchecked)

One instruction assigns the whole struct. What each field gates, and what the read sites impose
that the setter does not:

- `price_divergence.mark_oracle_percent_divergence` (`PERCENTAGE_PRECISION`, default 10%).
  Bounds the vAMM-vs-5min-oracle-TWAP spread for PnL settlement, fee sweeps, deficit resolution
  (`controller/orders.rs:1557-1611` via `validate_market_within_price_band`) and the funding
  gate (`math/oracle.rs:262,313-316`). It is not on the taker fill path. The reader floors the
  effective value at 10% (`math/oracle.rs:25-33`); no ceiling exists, so a large value disables
  the band. Lowering has no effect below the floor.
- `price_divergence.oracle_twap_5min_percent_divergence` (default 50%). Aborts fills, triggers,
  signed-message placement, and every liquidation and bankruptcy path with `PriceBandsBreached`
  when the live oracle strays from its own 5-min TWAP (`math/orders.rs:532-554`;
  `controller/liquidation.rs:375,1057,1792,1810,2377,2395`), and bands fill prices
  (`math/orders.rs:468-520`). The reader floors it at 50%. The dangerous direction is down: at
  the floor it can already block liquidation of underwater users on a fast-moving market. No
  ceiling; values above i64::MAX fail as `CastingFailure` at read time.
- `validity.slots_before_stale_for_amm` (i64 slots, default 10). The default AMM low-risk
  staleness threshold wherever the per-market override is 0 (`math/oracle.rs:417-421`), which is
  all spot markets and any perp market left at init default. **The trap:** `get_oracle_status`
  narrows it to i8 (`math/oracle.rs:292`), so any value outside -128..=127 makes every
  `update_funding_rate` revert with `CastingFailure`, halting funding accrual protocol-wide. A
  value <= -1 freezes AMM low-risk fills even on a same-slot price.
- `validity.slots_before_stale_for_margin` (i64 slots, default 120). Past it, oracles are
  `StaleForMargin`, which flips the margin calculation's validity flags
  (`math/margin.rs:301-302,651-659`) and with them withdrawals and risk-increasing actions.
  Liquidations, matched fills, PnL settlement, and triggers tolerate this verdict
  (`math/oracle.rs:209-232`), so lowering it does not block liquidating a stale-priced account.
- `validity.confidence_interval_max_size` (`BID_ASK_SPREAD_PRECISION`, default 2%). Multiplied
  by the market's tier-keyed 1x-50x confidence multiplier (`state/perp_market.rs:646-656`,
  `state/spot_market.rs:405-413`) and compared against the oracle's reported confidence
  (`math/oracle.rs:362-369`). Exceeding it is `TooUncertain`: fills, margin validity, and PnL
  settlement reject; liquidations and triggers tolerate. 0 blocks all of the former while still
  allowing liquidations.
- `validity.too_volatile_ratio` (unitless i64, default 5). Trips `TooVolatile` when
  `max(price, twap) / min(price, twap)` exceeds it as an integer quotient
  (`math/oracle.rs:358-360`). `TooVolatile` rejects everything except TWAP and AMM-curve
  updates, so it halts the market. Any value <= 0 trips on every read with a positive price,
  freezing the market entirely; 0 is also the struct's `#[derive(Default)]` zero-value.

### vAMM quote management hot role (program bounded)

The `VammQuoteManagement` hot role can update only the six active quoting instructions below.
Calls signed only by that role must remain inside protocol wide constants compiled into the
program. Warm/cold calls bypass these hot role limits while retaining the setters' original
semantic checks. There is no per market configuration or mutable baseline for a compromised hot
key to widen or ratchet.

| Managed value | Existing semantic range | Hot role range |
| --- | --- | --- |
| `amm.curve_update_intensity` | `0..=200` | `100..=150` |
| `amm.reference_price_offset_deadband_pct` | `0..=100` | `0..=25` |
| `amm.amm_jit_intensity` | `0..=100` | `0..=25` |
| `amm.max_spread` | `base_spread..=margin_ratio_initial * 100` | `10_000..=20_000`, plus the market dependent semantic range |
| `amm.amm_spread_adjustment` + `amm.amm_inventory_spread_adjustment` | full i8 for warm/cold (the setter has no semantic check) | both `-100..=100`, checked atomically |
| `amm.funding_bias_sensitivity` | full u8 | `0..=100` |

Oracle selection/validity, base spread, reserve/peg/k controls, MM-oracle state, and the low-CU
native spread bot remain outside this role.

### Per-market slot delay overrides (warm, unchecked)

Both are i8 fields on `PerpMarket`, read on every fill, trigger, signed-message placement, AMM
refresh, and margin calculation. `oracle_low_risk_slot_delay_override` replaces the state-level
AMM threshold when nonzero; every negative value clamps to a threshold of 0 (same-slot price
required), which blocks AMM low-risk fills outright. `oracle_slot_delay_override` gates
immediate (JIT, auction-skipping) AMM fills: 0 disables them for the market; negative means
unset (resolving to a 2-slot gap for MM-sourced prices, `constants.rs:281`); a high positive
value lets JIT fills execute against a price up to 127 slots old (plus up to 2 slots of hidden
MM-oracle source age, `constants.rs:292-299`), exposing counterparties to stale-price
arbitrage. These controls remain warm-only and are deliberately excluded from the vAMM
`VammQuoteManagement` role, along with global oracle guard rails and oracle identity setters.

The field doc comment on `oracle_low_risk_slot_delay_override`
(`state/perp_market.rs:451-453`) still describes an auction speed-bump override; the code uses
it purely as an oracle-staleness threshold. Treat the doc comment as stale.

### Prelaunch oracle (warm)

`update_prelaunch_oracle_params` sets the synthetic price directly and, in the same instruction,
overwrites the market's mark TWAPs (`admin.rs:3270-3280`), moving every open position's
TWAP-based margin, funding, and price-band math at once. The write is not durable: the next
permissionless crank recomputes the price from `min(last_mark_price_twap, max_price)`
(`state/oracle.rs:717-756`), so `max_price` is the only standing admin-side ceiling on what is
otherwise a self-referential feed (the market's own trading sets the TWAP that sets the price).
A negative price passes `validate()` but freezes the market rather than mis-pricing it: every
action rejects a `NonPositive` oracle (`math/oracle.rs:356,431-432`).

### MM-oracle kill switch

Clearing the `MmOracleUpdate` bit stops both native MM-oracle crank handlers from storing new
prices (`admin.rs:3615-3618,3907-3910`). Positions do not freeze; pricing falls back to the
exchange oracle once the stored MM price ages past it (`state/oracle.rs:333-357`), and
immediate-fill staleness tightens. Disabling needs only the `FeatureFlag` hot key; re-enabling
requires the cold admin exactly. The MM-oracle write path itself (step caps, slot gaps) is
governed by hard-coded constants, not admin parameters (`constants.rs:281-299`).

## Market status, pauses, and kill switches

This family has the widest spread of authority tiers: the exchange-wide and per-market pause
bitmasks admit the pause admin (add-only), feature kill switches sit on a hot key (disable-only,
with cold-only re-enable), one circuit breaker is fully permissionless, and the solvency switch
is cold-only.

| Field | Set by | Tier | Units | Enforced bounds |
| --- | --- | --- | --- | --- |
| `State.exchange_status` | `update_exchange_status` (`admin.rs:3144`) | pause | u8 bitmask, 8 `ExchangeStatus` bits (`state/state.rs:124-136`) | pause admin add-only (`admin.rs:3150`); warm/cold set anything; all 8 bits defined |
| `PerpMarket.status` | `update_perp_market_status` (`admin.rs:2314`) | warm | `MarketStatus` enum | Settlement and Delisted rejected (`admin.rs:2318-2322`); no transition checks within {Initialized, Active, ReduceOnly} |
| `SpotMarket.status` | `update_spot_market_status` (`admin.rs:1934`) | warm | `MarketStatus` enum | **unchecked**: any of the 5 variants, Delisted included; see below |
| `PerpMarket.paused_operations` | `update_perp_market_paused_operations` (`admin.rs:2341`) | pause | u8 bitmask, 8 `PerpOperation` bits (`state/paused_operations.rs:6-16`) | three-way: cold anything; warm only the `UpdateFunding`/`SettleRevPool` bits (`admin.rs:2353-2363`); pause admin add-only (`admin.rs:2364-2369`) |
| `SpotMarket.paused_operations` | `update_spot_market_paused_operations` (`admin.rs:1954`) | pause | u8 bitmask, 5 `SpotOperation` bits (`state/paused_operations.rs:57-64`) | pause admin add-only (`admin.rs:1963-1968`); warm/cold anything; undefined bits 5-7 stored but inert |
| `SpotMarket.paused_operations` (Deposit and Withdraw bits) | `pause_spot_market_deposit_withdraw` (`keeper.rs:3399`) | **anyone** | the two bits, OR-only | only succeeds when the vault invariant is provably broken (`keeper.rs:3405-3411`); cannot clear anything |
| `SpotMarket.expiry_ts` + status | `update_spot_market_expiry` (`admin.rs:1103`) | warm | Unix seconds | expiry must be in the future (`admin.rs:1111-1115`); status hard-coded to ReduceOnly (`admin.rs:1129`) |
| `PerpMarket.expiry_ts` + status | `update_perp_market_expiry` (`admin.rs:1138`) | warm | Unix seconds | expiry must be in the future (`admin.rs:1146-1150`); status hard-coded to ReduceOnly (`admin.rs:1164`) |
| `SpotMarket.orders_enabled` | `update_spot_market_orders_enabled` (`admin.rs:2271`) | warm | bool | **unchecked**, and dead: no program code reads it (see below) |
| `State.solvency_status` | `update_solvency_status` (`admin.rs:3160`) | **cold** | u8, only bit 0 defined (`state/state.rs:147-152`) | **unchecked**: any u8 stored; values >= 2 brick the readers (see below) |
| `UserStats.paused_operations` | `admin_update_user_stats_paused_operations` (`admin.rs:3202`) | pause (or hot `UserFlag`) | u8 bitmask, 3 bits (`state/user.rs:2176-2183`) | pause admin add-only (`admin.rs:3216-3222`); cold/warm/hot-UserFlag set anything |
| `State.feature_bit_flags`: MedianTriggerPrice, BuilderCodes | `update_feature_bit_flags_*` (`admin.rs:4147,4168`) | hot (`FeatureFlag`) to disable; **cold to enable** | one bit each (`state/state.rs:406-412`) | single-bit read-modify-write; asymmetric authority in-handler (`admin.rs:4153-4157,4174-4178`) |
| `State.lp_pool_feature_bit_flags`: SettleLpPool, SwapLpPool, MintRedeemLpPool | `update_feature_bit_flags_*_lp_pool` (`admin.rs:4210,4231,4252`) | hot to disable; cold to enable | one bit each (`state/state.rs:414-419`) | same asymmetry; consumers compiled out of mainnet builds (`vlp-hedge` feature) |
| `PerpMarket.hedge_config.paused_operations` | `update_perp_market_lp_pool_paused_operations` (`admin.rs:3019`) | pause | u8 bitmask, 2 `PerpLpOperation` bits (`state/paused_operations.rs:117-121`) | pause admin add-only (`admin.rs:3027-3032`); no Delisted gate |
| `PerpMarket.hedge_config.status` | `update_perp_market_lp_pool_status` (`vlp/hedge/admin.rs:908`) | warm | u8, read as zero/nonzero | **unchecked**; the setter is compiled out of mainnet (`lib.rs:985-986`) while the readers stay in |

### What a change does

**Exchange status** (`update_exchange_status`, pause tier). The exchange-wide kill switch. Each
bit fails its operation across every market at once via the per-instruction guards in
`instructions/constraints.rs:85-170` (deposit, withdraw, AMM, fill, liquidation, funding,
settle-PnL, immediate AMM fill). A user's next matching instruction fails with `ExchangePaused`.
The pause admin can only add bits; unpausing needs warm or cold.

**Perp market status.** ReduceOnly forces every fill in the market to be risk-reducing
(`controller/orders.rs:175,1106,1675`); positions can still be closed. Initialized is much
harsher than it sounds: order placement fails *and* the fill gate (Active|ReduceOnly only,
`controller/orders.rs:1080-1087`) fails, so existing positions cannot be closed through the book
either. Settlement and Delisted cannot be set here; they are written only by
`settle_expired_market` (`vlp/amm/refresh.rs:573`) and the pools-settlement instruction
(`admin.rs:1313`).

**Spot market status is the sharpest edge in this family.** The handler writes any status with
no validation, including Delisted. Withdrawals admit only Active|ReduceOnly|Settlement
(`controller/spot_position.rs:157-165`), so writing Delisted (or Initialized) blocks all
withdrawals in the market. And because every writer of spot status carries
`#[access_control(spot_market_valid)]`, which rejects a Delisted market, **Delisted is a one-way
door for a spot market**: one warm-tier transaction can permanently strand every user's
collateral in that market. This directly contradicts the comment at
`controller/isolated_position.rs:520-532`, which justifies the withdrawal gate on the claim that
an admin can move a market out of Delisted again. See
[missing bounds](#missing-bounds-worth-knowing-about).

**Expiry setters.** Both flip the market to ReduceOnly immediately and set a future expiry. On
the perp side, once the clock passes `expiry_ts` the market closes to new risk on every path,
and there is a window trap: between `expiry_ts` and a keeper running `settle_expired_market`,
liquidation of the market is refused (`controller/liquidation.rs:171-185,876-890`), so an
underwater expired position cannot be liquidated in that window. Nothing enforces a minimum lead
time. On the spot side, `expiry_ts` has exactly one reader (the revenue-pool deposit gate,
`instructions/user.rs:3754-3759`); the user-visible effect of the instruction is entirely the
ReduceOnly flip, and there is no spot settlement machinery behind the timestamp.

**Per-market pause bits.** Perp bits block their operation live: `Fill` stops resting orders
filling, `UpdateFunding` freezes funding accrual, `SettlePnl*` blocks settlement, `Liquidation`
refuses liquidations (leaving underwater positions un-liquidatable until cleared), the AMM bits
withdraw the vAMM as counterparty. Warm callers who are not cold can only toggle
`UpdateFunding` and `SettleRevPool` (`state/paused_operations.rs:33-34`); everything else needs
cold, or add-only via the pause admin. Spot bits gate deposits, withdrawals, swap legs,
IF unstaking, and the two legs of spot liquidations; there is no warm bit budget on the spot
side. The permissionless `pause_spot_market_deposit_withdraw` can OR in the Deposit and Withdraw
bits, but only when the spot vault invariant is genuinely violated on-chain; any signer can trip
it, and only warm or cold can clear it.

**One authority caveat spans all the pause bitmasks:** `require_pause_only_added` short-circuits
when the signer is warm (`auth.rs:118`), and the perp handler's bit-budget check is skipped for
the pause admin (`admin.rs:2356`). A single key holding both the warm and pause roles therefore
bypasses both restrictions. The documented matrix holds only while the keys are distinct.

**Solvency status** (`update_solvency_status`, cold, **unchecked**). Bit 0 pauses all three
solvency-repair instructions (PnL deficit resolution and both bankruptcy paths,
`instructions/keeper.rs:2091-2094,2257-2260,2406-2409`); a bankrupt user stays bankrupt until it
clears. The missing bound has a self-inflicted-DoS consequence: the readers decode the byte with
`BitFlags::from_bits(...)` and only bit 0 is defined, so any stored value >= 2 makes all three
repair instructions fail with `FailedUnwrap` until the cold admin writes 0 or 1 back
(`state/state.rs:246-248`, `math/safe_unwrap.rs:32-47`). Other bitmask setters guard against
unknown bits (`admin.rs:2227-2234,4315-4322`); this one does not.

**Per-user throttles** (`admin_update_user_stats_paused_operations`). Three bits on a single
user's `UserStats`: force their market orders through the full auction instead of atomic vAMM
fills (`state/user.rs:817-836`), restrict atomic fills to reduce-only, or stop their fills
moving the bid/ask TWAP (`state/user.rs:2153-2159`).

**Feature kill switches.** All follow one pattern: any holder of the `FeatureFlag` hot key (or
warm/cold) can *disable*; only the cold admin can *re-enable*. `MedianTriggerPrice` changes
which reference price trigger orders evaluate against (`controller/orders.rs:3799`,
`state/perp_market.rs:1102-1121`), so the same oracle print can trigger a stop under one setting
and not the other. `BuilderCodes` gates builder-fee escrow and revenue share on every order and
fill. The three LP-pool bits gate LP-pool settlement, swaps, and mint/redeem; mint and redeem
share one bit, so there is no way to close entry while leaving the exit open. Note the LP-pool
consumers are compiled out of mainnet builds (`vlp-hedge` feature), so on mainnet those three
bits currently have no reachable reader.

**Dead switch:** `SpotMarket.orders_enabled` has no reader anywhere in the program (declaration,
default, initializer, and setter are the only references). The SDK admin wrapper's doc comment
(`packages/sdk/src/adminClient.ts:3776`) claims it enables or disables spot limit orders; on
chain it does nothing. Spot DLOB trading is disabled by a hard-coded rejection instead
(`controller/orders.rs:835-843`).

**LP-pool hedge config on mainnet:** `hedge_config.status` (unchecked, warm) kills both LP-pool
revenue tracking and quote-owed settlement for a market when 0, silently (loop skips, not
reverts). Its setter is feature-gated out of mainnet builds while the field and readers ship, so
on mainnet the value is frozen at whatever it holds.

## Order sizing, auctions, and pools

The perp order-parameter setters carry partial bounds; every spot-side sibling is inert today
because spot DLOB trading is hard-disabled by an unconditional rejection
(`controller/orders.rs:835-843`, `SpotDlobTradingDisabled`). This section says which is which.

| Field | Set by | Tier | Units | Enforced bounds |
| --- | --- | --- | --- | --- |
| `PerpMarket.order_step_size` | `update_perp_market_step_size_and_tick_size` (`admin.rs:2747`) | warm | `BASE_PRECISION` (1e9) | 1..=2_000_000_000 (`admin.rs:2755-2756`); no divisibility re-check against existing state (see below) |
| `PerpMarket.order_tick_size` | same handler | warm | `PRICE_PRECISION` (1e6) | > 0 only (`admin.rs:2755`); no upper bound |
| `PerpMarket.market_stats.min_order_size` | `update_perp_market_min_order_size` (`admin.rs:2778`) | warm | `BASE_PRECISION` | > 0 only (`admin.rs:2785`); no upper bound, no tie to step size |
| `PerpMarket.max_open_interest` | `update_perp_market_max_open_interest` (`admin.rs:2858`) | warm | `BASE_PRECISION` (u128); 0 = uncapped | must be a multiple of the current step size and fit u64 (`admin.rs:2865-2872`) |
| `State.min_perp_auction_duration` | `update_perp_auction_duration` (`admin.rs:3174`) | warm | slots (u8) | **unchecked** (0-255) |
| `State.default_spot_auction_duration` | `update_spot_auction_duration` (`admin.rs:3188`) | warm | slots (u8) | **unchecked**, and no on-chain reader exists |
| `SpotMarket.order_step_size` / `order_tick_size` | `update_spot_market_step_size_and_tick_size` (`admin.rs:2800`) | warm | token precision / `PRICE_PRECISION` | > 0 required, except **market 0 is fully exempt** (`admin.rs:2808-2811`); effectively inert (spot DLOB disabled) |
| `SpotMarket.min_order_size` | `update_spot_market_min_order_size` (`admin.rs:2833`) | warm | token precision | > 0 except market 0 exempt (`admin.rs:2840-2843`); no on-chain reader |
| `SpotMarket.pool_id` | `update_spot_market_pool_id` (`admin.rs:462`) | warm | u8 pool tag (0 = main, 2 = LST, `constants.rs:268`) | value **unchecked**; market status must be Initialized (`admin.rs:473-477`) |
| `PerpMarket.hedge_config.pool_id` | `update_perp_market_lp_pool_id` (`admin.rs:1716`) | warm | u8 LP-pool id | **unchecked**; no cross-check against any LPPool account |

### What a change does

**Perp step size is retroactively enforced against existing state.** New orders must be at least
one step and are rounded down to a step multiple (`controller/orders.rs:201-222`,
`math/orders.rs:237-246`). But the current step size is also an *invariant* on state that already
exists: `validate_perp_market` requires the market's aggregate long/short base amounts to be
exact multiples (`validation/perp_market.rs:13-30`) and runs inside every fill
(`controller/orders.rs:1184`) and settle-PnL (`controller/pnl.rs:419-423`);
`validate_perp_position_with_perp_market` does the same per position
(`validation/position.rs:22-29`). A value inside the legal 1..=2e9 range that does not divide the
existing aggregates therefore halts all fills and PnL settlement in the market until reverted.
Nothing at write time re-checks divisibility of existing positions, aggregates, or
`max_open_interest` (whose own multiple-of-step check runs only when *it* is written,
`admin.rs:2865-2872`).

**Perp tick size is read live by resting orders.** An auction or oracle-offset order's effective
limit price is recomputed from the *current* tick size on every fill attempt
(`state/user.rs:1521-1550`, `state/fill_mode.rs:24-47`), and the bid/ask TWAP crank reprices
resting makers with it (`math/orders.rs:1213`), so a tick change reprices the resting book and
feeds funding at the next crank, not just new placements. With no upper bound, a large tick has
concrete failure modes: post-only Slide placement underflows (`math/orders.rs:333,339`), the
AMM fill-sizing helper underflows (`math/orders.rs:176`), and oracle-offset limit prices are
silently floored at one tick (`state/user.rs:1543`).

**Perp min order size** floors non-reduce-only placements (`validation/order.rs:334-340`) and,
less obviously, anchors the funding-rate depth clamp
(`state/perp_market.rs:1001-1008`: OI/1000 clamped into [100x, 5000x] of it) and AMM fallback
pricing and k-lowering (`vlp/amm/state.rs:545-598,774-783`, `vlp/amm/quoter.rs:649`). Raising it
does not cancel resting orders below the new floor. A value above u64::MAX/5000 makes the depth
helper error and takes funding down with it.

**Max open interest** gates risk-increasing placement when nonzero
(`controller/orders.rs:422-442`) and is re-checked *unconditionally after every fill*
(`controller/orders.rs:1513-1526`, strict >). Setting it to a nonzero value at or below current
open interest therefore reverts every fill in the market, position-reducing fills included,
until raised or zeroed. Existing positions are never force-reduced.

**Perp auction duration** (`min_perp_auction_duration`, **unchecked**). Floors the auction
length of every market and oracle order, and of limit orders that request an auction
(`controller/orders.rs:498-561`); the duration also stretches the order's `max_ts`. It is
latched onto the order at placement, so a change affects newly placed orders only. Triggered
stop orders are unaffected; their path hardcodes a floor of 20 slots
(`controller/orders.rs:3819-3825`, relied on by the TWAP-manipulation defense at
`constants.rs:223-237`). At 255 slots (~102s) every market order's auction stretches
accordingly; nothing bounds it.

**Pool partition tags.** `SpotMarket.pool_id` is the sharp one: users can only deposit,
withdraw, transfer, be margined, and be liquidated against spot markets whose `pool_id` matches
their `User.pool_id` (`math/margin.rs:289-297`, `instructions/user.rs:590-596,1347-1352`,
`controller/liquidation.rs:1358-1381,2016-2020`). The status-must-be-Initialized gate looks like
a protection but is bypassable by the same warm admin in two transactions (set status back to
Initialized via the unchecked status setter, change the pool, restore Active). Repointing a live
market makes `calculate_margin_requirement` fail for every user holding it in the old pool: they
cannot withdraw, place orders, or be liquidated until the value is restored.
`PerpMarket.hedge_config.pool_id` is unchecked and uncross-checked; a wrong value hard-fails the
whole LP-pool hedge settle batch (`vlp/hedge/settle.rs:87-95`) rather than skipping the market.
On mainnet builds this consumer is compiled out (`vlp-hedge` feature), so the write is currently
inert there.

**Inert spot knobs.** The three spot order-parameter fields and
`State.default_spot_auction_duration` have no live reader while spot DLOB trading is disabled
(the one surviving read path always ends in `SpotDlobTradingDisabled`). Their setters also share
a quirk worth knowing if spot trading is ever re-enabled: the quote market (index 0) is exempt
from the > 0 checks, so 0 is writable for it, which would surface as `MathError`s in the sizing
helpers. Whether these knobs are reserved for a spot re-enable or are dead config is a team
question, not something the code answers.

## Fees

Where the fees *go* is documented in [FEES.md](../FEES.md); this section covers only the bounds
on the setters and what changes for a user mid-flight. The two `FeeStructure` setters share one
thorough validator, `validate_fee_structure` (`validation/fee_structure.rs:15-137`).

| Field | Set by | Tier | Units | Enforced bounds |
| --- | --- | --- | --- | --- |
| `State.perp_fee_structure` (10 fee tiers, filler rewards, AMM/IF split) | `update_perp_fee_structure` (`admin.rs:2509`) | warm | tier fees in tenths of a bp (numerator over denominator >= `FEE_DENOMINATOR` 100_000, `constants.rs:159`); filler/referral in percent; `flat_filler_fee` in `QUOTE_PRECISION` | taker fee <= 30 bps; maker rebate <= 3 bps; referee/referrer <= 100%; filler reward <= 20%; `flat_filler_fee` <= $0.01; `amm + if` numerators <= 100; two derived consistency checks (fill must leave the market at least the maker rebate) (`validation/fee_structure.rs:15-137`). No lower bounds: an all-zero schedule is legal |
| `State.spot_fee_structure` | `update_spot_fee_structure` (`admin.rs:2525`) | warm | same | same validator; almost entirely inert (see below) |
| `PerpMarket.fee_adjustment` | `update_perp_market_fee_adjustment` (`admin.rs:2887`) | warm | signed percent of the base fee, i16 | -100..=100 (`admin.rs:2894-2900`, `FEE_ADJUSTMENT_MAX` `constants.rs:187`) |
| `PerpMarket.taker_fee_addon_tenth_bps` | `update_perp_market_taker_fee_addon` (`admin.rs:2912`) | warm | tenth-bps (u16, unsigned: surcharge only); field `state/perp_market.rs:307` | <= 100 tenth-bps = 10 bps (`admin.rs:2919-2924`, `MAX_TAKER_FEE_ADDON_TENTH_BPS` `constants.rs:171`) |
| `State.promo_fee_tier` | `update_promo_fee_tier` (`admin.rs:2481`) | warm | perp fee-tier index (u8); 0 disables | <= highest populated tier, `PERP_FEE_TIER_MAX_INDEX` = 2 (`admin.rs:2491-2496`, `constants.rs:177`) |
| `SpotMarket.fee_adjustment` | `update_spot_market_fee_adjustment` (`admin.rs:3081`) | warm | same as perp `fee_adjustment` | same bound; **no reader exists** (see below) |
| `PerpMarket.fee_pool_buffer_target` | `update_perp_market_fee_pool_buffer_target` (`admin.rs:2937`) | warm | `QUOTE_PRECISION` | **unchecked** |
| `PerpMarket.bankruptcy_if_floor_pct` | `update_perp_market_bankruptcy_if_floor_pct` (`admin.rs:2957`) | warm | `PERCENTAGE_PRECISION` (u32); 0 disables | <= 1_000_000 = 100% of OI notional (`admin.rs:2964-2968`) |
| `State.feature_bit_flags` VammMakerRebate bit | `update_feature_bit_flags_vamm_maker_rebate` (`admin.rs:4189`) | hot (`FeatureFlag`) to disable; **cold to enable** (`admin.rs:4195-4199`) | one bit | single-bit read-modify-write |
| `State.protocol_fee_recipient_perp` / `_spot` | `update_protocol_fee_recipient` (`admin.rs:4940`) | **cold** | Pubkey | **unchecked** (any 32 bytes, `Pubkey::default()` included) |

### What a change does

**Perp fee structure.** Rewrites the global schedule read on every perp fill
(`controller/orders.rs:1422` into `math/fees.rs:78,384`): taker fee, maker rebate, filler
rewards, and the AMM/IF/protocol split of the remainder. A resting maker order or in-flight
taker order is priced under whichever schedule is live at the instant it fills; nothing is
grandfathered. `flat_filler_fee` also sets the keeper reward for expired, triggered, and
force-cancelled orders (`controller/orders.rs:1325,1378,1469,3948,4144`). The array holds 10
tier slots but only the first `PERP_FEE_TIER_MAX_INDEX + 1` = 4 are populated; which tier a
taker gets is not admin-settable: the thresholds are hardcoded ($5M, $80M and $200M of
trailing-30d volume, `math/fees.rs` `VOLUME_THRESHOLDS`), evaluated at every fill against a live-decayed volume
projection (`state/user.rs:2105`), so schedule changes bite at the account's next fill.

**Per-market fee adjustment** scales the taker fee and maker rebate by up to +/-100% on every
fill in that market (`math/fees.rs:238-306`) and shifts the post-only limit-price buffer
(`math/orders.rs:129-160`), all read live. On the taker side the percentage applies to the sum
of the tier fee and the market's taker-fee add-on (`math/fees.rs:230-236`); the maker rebate
sees the adjustment alone.

**Taker fee add-on** (`update_perp_market_taker_fee_addon`) is an absolute per-market
surcharge in tenth-bps, added to the tier fee before `fee_adjustment` scales the sum
(`math/fees.rs:238-256`). It exists because the multiplicative `fee_adjustment` cannot express
a flat markup across tiers. It is unsigned, so it can only raise the taker fee (a discount
needs `fee_adjustment` or the promo tier), it never touches the maker rebate or the post-only
buffer, and it is capped at 10 bps. Read live: a resting taker order pays the add-on in force
at the instant it fills.

**Promo fee tier** (`update_promo_fee_tier`) floors every account's effective perp fee tier at
`max(volume tier, promo_fee_tier)` while nonzero (`math/fees.rs:503-524`). Nobody is
downgraded by it, there is no per-user state, and setting 0 returns every account to its
volume tier at its next fill. The bound is checked against the highest *populated* tier
because the tier function clamps: an unbounded value would validate and then silently mean a
lower tier (the handler comment at `admin.rs:2487-2490` records this).

**Fee pool buffer target** (**unchecked**) is the retention margin the insurance-fund and
AMM-provision drains must leave in the market's fee pool (`controller/perp_pools.rs:151-152`);
the protocol drain is buffer-exempt and runs first. It changes sweep *timing*, never user
entitlement; an arbitrarily large value silently stalls the IF sweep and AMM-provision
tokenization for the market (saturating subtraction, no error), even at delisting. It is also
the one fee setter with no Delisted-market gate. See FEES.md's sweep-buffer discussion.

**Bankruptcy IF floor** sizes the standing first-loss tranche the sweeps must leave backed:
`open_interest x oracle TWAP x pct` (`state/perp_market.rs:938-982`), reserved at
`controller/perp_pools.rs:130` and `controller/revenue_share.rs:82`. Raising it delays (never
diverts) the IF's cut. The dollar floor moves with the latched oracle TWAP, not the live price.
Bankruptcy resolution itself consumes the `pending_if_fee` counter, not this field.

**vAMM maker rebate bit** shifts part of the taker-fee remainder into the AMM's fee pool on
AMM-path fills only (`math/fees.rs:190-199`); DLOB match fills never see it. The rebate is
sized from tier 0 of the fee schedule, not the taker's tier (`math/fees.rs:308-318`), and
clamped to the available remainder, so editing `fee_tiers[0]`'s maker-rebate numerators also
moves the vAMM's own rebate while the bit is on. Same asymmetric authority as the other
feature bits: hot key can turn it off, only cold can turn it on.

**Protocol fee recipient** (cold, **unchecked**) only constrains where the *next* treasury
withdrawal may send funds (`address =` constraints in
`instructions/protocol_fees/withdraw_protocol_fees_{perp,spot}.rs`); accrued balances do not
move. The zero pubkey blocks withdrawals via an explicit guard on the consumer. Nothing checks
the recipient can actually sign: a mistyped authority still receives tokens into a freshly
created ATA it may never be able to move.

**Inert spot fee surface.** Spot DLOB trading is deleted in this fork (no spot fill
instructions; placement is rejected at `controller/orders.rs:835-843`), so the spot fee tiers,
filler-reward structure, and AMM/IF numerators have no live reader, and
`SpotMarket.fee_adjustment` has none at all. The only live read of `spot_fee_structure` is
`flat_filler_fee`, charged when force-cancelling a legacy resting spot order
(`controller/orders.rs:4118`). The operator hazard is a false sense of control: changing spot
fees reprices nothing.

## State-level globals, funding, and admin keys

The grab bag with the highest variance: it contains both the best-behaved gate in the program
(the PnL-pool credit, which is solvency-checked after the fact) and the worst single missing
bound found in this review (the funding period).

| Field | Set by | Tier | Units | Enforced bounds |
| --- | --- | --- | --- | --- |
| `PerpMarket.market_stats.funding_period` | `update_perp_market_funding_period` (`admin.rs:1480`) | warm | seconds (i64) | >= 0 only (`admin.rs:1491`); downstream math imposes a hidden functional ceiling (see below) |
| `PerpMarket.funding_clamp_threshold` | `update_perp_market_funding_dead_zone` (`admin.rs:1506`) | warm | bps (`BPS_PRECISION` 10000) | < 10000, strict (`admin.rs:1519-1522`) |
| `PerpMarket.funding_ramp_slope` | same handler | warm | multiplier at `PERCENTAGE_PRECISION`; default 1.0x | > 0 only (`admin.rs:1524`); no upper cap (u32, up to ~4294x) |
| `PerpMarket.pnl_pool` (credit) | `update_perp_market_pnl_pool` (`admin.rs:1321`) | warm | quote token amount | no numeric cap, but the credit must leave the vault covering all depositor claims (`admin.rs:1337`, `math/spot_withdraw.rs:532-547`); no tokens move |
| `PerpMarket.name` | `update_perp_market_name` (`admin.rs:1616`) | warm | [u8; 32] | **unchecked** |
| `SpotMarket.name` | `update_spot_market_name` (`admin.rs:1629`) | warm | [u8; 32] | reserved name "USDT" only on market 0 (`admin.rs:1634-1639`); otherwise anything |
| `SpotMarket.asset_tier` | `update_spot_market_asset_tier` (`admin.rs:1981`) | warm | `AssetTier` enum (5 variants, `state/spot_market.rs:750-762`) | must be Collateral/Protected while `initial_asset_weight > 0` (`admin.rs:1988-1994`); otherwise any variant |
| `State.settlement_duration` | `update_state_settlement_duration` (`admin.rs:2600`) | warm | seconds (u16) | **unchecked** (0 to ~18.2h) |
| `State.max_number_of_sub_accounts` | `update_state_max_number_of_sub_accounts` (`admin.rs:2614`) | warm | count (u16); values > 5 mean value x 100 (`state/state.rs:256-262`); 0 = unlimited | **unchecked** |
| `State.max_initialize_user_fee` | `update_state_max_initialize_user_fee` (`admin.rs:2628`) | warm | hundredths of 1 SOL (u16) | **unchecked** (up to 655.35 SOL) |
| `PerpMarket.number_of_users` / `number_of_users_with_base` | `update_perp_market_number_of_users` (`admin.rs:2980`) | warm | counts (u32) | only `users >= users_with_base` (`admin.rs:3010-3014`); no check against the true count; works on any market status |
| `PerpMarket.market_config` (DisableFormulaicKUpdate bit) | `update_perp_market_config` (`admin.rs:4276`) | warm to clear; **cold-key to set** (`admin.rs:4292-4298`) | u8, 1 defined bit | unknown bits rejected (`admin.rs:4282-4287`) |
| `User.special_user_status` (VammHedger bit) | `update_special_user_status` (`admin.rs:4311`) | hot (`UserFlag`) to clear; **cold-key to set** (`admin.rs:4326-4332`) | u8, 1 defined bit | unknown bits rejected (`admin.rs:4317-4322`) |
| `State.discount_mint` | `update_discount_mint` (`admin.rs:3130`) | warm | Pubkey | **unchecked**, and no reader exists (vestigial) |
| `State.whitelist_mint` | `update_whitelist_mint` (`admin.rs:3116`) | none: **not callable** | Pubkey | the lib.rs wrapper is commented out (`lib.rs:1691-1696`); the field is frozen at `Pubkey::default()` (whitelist off) |
| `State.cold_admin` | `update_admin` (`admin.rs:3106`) | **cold** | Pubkey | **unchecked** (`Pubkey::default()` accepted) |
| `State.warm_admin` | `update_warm_admin` (`admin.rs:4901`) | **cold** | Pubkey | **unchecked** |
| `State.pause_admin` | `update_pause_admin` (`admin.rs:4911`) | **cold** | Pubkey | **unchecked** |
| `State.hot_<role>` (12 roles) | `update_hot_admin` (`admin.rs:4925`) | warm | Pubkey per `HotRole` (`state/state.rs:109-122`) | **unchecked** |

### What a change does

**Funding period is the worst missing bound in the program.** The setter checks only `>= 0`, but
the value is read on *every fill* (funding updates run at the end of every trade,
`controller/orders.rs:1541-1550`), and the downstream math has two revert bands: above 86,400s
the period adjustment integer-divides to 0 and the next due funding update returns `MathError`
(`controller/funding.rs:361-391`), and above i64::MAX/2 the schedule helper overflows on every
call (`math/helpers.rs:87`). Either way the error propagates through the fill path, so **every
fill in the market fails** until the value is corrected. 0 is also legal and quietly collapses
the mark-TWAP window to the last print (`math/stats.rs:87`), defeating the mark/oracle
divergence rails, and divides by zero in the median-trigger-price basis when that feature is on
(`state/perp_market.rs:1182-1195`). A larger period also *raises* the per-interval funding rate
(the period adjustment is a divisor), the reverse of the intuition that longer means gentler.

**Funding dead zone and ramp.** The clamp threshold carves a no-premium band around the oracle
TWAP (`controller/funding.rs:374-377`, `math/funding.rs:79-97`); widening it can zero the
premium component of funding for every open position. The ramp slope scales the premium past
the band; it is uncapped, but the result is clamped to the market's max price spread before
becoming the rate (`controller/funding.rs:386`), so an oversized slope saturates rather than
runs away (an extreme slope with an extreme spread can still fail the narrowing cast at
`math/funding.rs:94` and revert).

**PnL pool credit.** Not a token deposit: it re-attributes existing quote-vault surplus into
the market's PnL pool, and the only bound is the after-the-fact invariant that the vault still
covers all depositor claims (`math/spot_withdraw.rs:536-544`). It raises how much positive PnL
becomes claimable at the next settle (`controller/pnl.rs:286-306`).

**Asset tier is an oracle-tolerance knob in disguise.** Beyond gating borrows
(Protected bans them, `controller/spot_position.rs:174-179`) and capping isolated liabilities
(`math/margin.rs:668-705`), the tier keys the market's accepted oracle confidence multiplier: 1x
for Collateral/Protected, 5x for Cross, 50x for Isolated/Unlisted
(`state/spot_market.rs:405-412`), and the TWAP sanitize clamp (`state/spot_market.rs:414-422`).
Moving a market from Collateral to Isolated widens its accepted oracle confidence from 2% to
100% in one transaction.

**Settlement duration** is the global buffer between a perp market's expiry and forced
settlement of expired positions (`controller/pnl.rs:547-556`). Unchecked, but the u16 domain
caps the delay at ~18.2 hours, and 0 unblocks already-expired positions immediately.

**Account creation knobs.** `max_number_of_sub_accounts` caps both sub-accounts and authorities
at creation (`instructions/user.rs:191-197,272-278`) with a scaling discontinuity (5 means 5,
6 means 600; nothing in between is expressible), and any nonzero value adds a 13-day age gate to
rent reclaim (`instructions/user.rs:3723-3731`). `max_initialize_user_fee` scales the
account-creation fee once account-space utilization passes 80% of that cap
(`state/state.rs:264-283`), and any nonzero value also gates deletion of fresh, non-idle
accounts (`validation/user.rs:62-73`). Both touch existing users only through those gates.

**User counters.** The two `number_of_users` overrides gate market deletion
(`admin.rs:911-916`) and the final delist sweep (`admin.rs:1213-1222`). Overriding
`number_of_users_with_base` downward can let the delist sweep run while base positions are
still outstanding; the automatic decrements are saturating, so an override set too low sticks
at 0 with the true count unrecoverable on-chain. The handler carries no market-status gate.

**Cold-to-set bit flags.** `market_config`'s DisableFormulaicKUpdate freezes the AMM's automatic
liquidity-depth (sqrt_k) adjustment on all three paths (repeg, formulaic update, funding-driven,
`vlp/amm/math/repeg.rs:290-293`, `vlp/amm/controller.rs:163-166`, `controller/funding.rs:274-275`).
`special_user_status`'s VammHedger bit admits a user to the privileged
vAMM position-transfer instruction (`instructions/user.rs:4687-4691`; note the consumer tests
exact equality, so a hypothetical second bit would silently revoke access). Both share a
two-tier gate: the accounts struct admits warm or hot, but *setting* a bit requires the literal
cold-admin key in the handler body.

**Admin key rotations.** All four rotation handlers are bare assignments; the tier hierarchy is
the protection, not any value check. Cold rotates itself, warm, and pause; warm rotates the 12
hot role keys. Rotations bite at the very next authority check with no timelock (the module doc
`auth.rs:1-14` places the timelock in off-chain key custody). Two asymmetries worth knowing:
setting `warm_admin` or a hot key to `Pubkey::default()` is recoverable (the tier collapses onto
cold), but setting `cold_admin` to `Pubkey::default()` or an unrecoverable key permanently
disables every cold-gated path, including the rotation handler itself, oracle swaps, and the
ability to ever set the cold-gated config bits. Nothing on-chain guards against it.

**Dead or inert.** `State.discount_mint` has no reader (an upstream fee-discount leftover, like
`State.srm_vault`). `State.whitelist_mint` cannot be changed at all: the instruction wrapper is
commented out of `lib.rs`, so the deposit whitelist is permanently off absent a program upgrade.
Market names have no on-chain reader; the one enforced rule (only market 0 may be named "USDT")
exists because off-chain monitoring keys its stablecoin exemption off the decoded name
(`admin.rs:172-178`). Perp names carry no such rule, so a perp rename can still mislead
dashboards.

## Missing bounds worth knowing about

Everything in this section was confirmed by reading the handler and every helper it calls, then
re-checked by a second pass that actively tried to find a validator. The doc stays descriptive;
whether any of these deserves a code change is a separate decision. Ordered by blast radius.

1. **`funding_period` can halt all fills in a market** (`admin.rs:1491` checks only `>= 0`).
   Values above 86,400s make the next due funding update revert inside the fill path
   (`controller/funding.rs:361-391`); values above i64::MAX/2 revert on every call
   (`math/helpers.rs:87`); 0 collapses the mark TWAP to the last print and defeats the
   divergence rails (`math/stats.rs:87`).
2. **Spot `MarketStatus::Delisted` is an irreversible withdrawal freeze.** The status setter
   accepts any variant unchecked (`admin.rs:1934-1949`), withdrawals reject Delisted
   (`controller/spot_position.rs:157-165`), and every writer of spot status refuses to run on a
   Delisted market (`constraints.rs:54-59`), so there is no way back. The comment at
   `controller/isolated_position.rs:520-532` claims the opposite and is wrong.
3. **Spot `liquidator_fee` has no relationship to the liability weights**
   (`admin.rs:1767-1774` bounds only the three-fee sum below 100%). A fee above the market's
   maintenance-weight headroom guarantees bad debt on every liquidation in it; the perp side has
   exactly this cross-check (`validation/margin.rs:34-42`) and the spot side has none.
4. **The six oracle guard rails are one unchecked struct write** (`admin.rs:2596`). Sharpest
   edges: `slots_before_stale_for_amm` outside -128..=127 bricks every funding update via an i8
   cast (`math/oracle.rs:292`); `too_volatile_ratio <= 0` freezes the market on every read, and
   0 is the struct's derive-default; the two divergence fields have read-side floors but no
   ceilings, so large values disable the bands.
5. **`update_solvency_status` accepts bytes its own readers cannot decode.** Only bit 0 is
   defined; any stored value >= 2 makes all three solvency-repair instructions fail with
   `FailedUnwrap` until rewritten (`state/state.rs:246-248`). The unknown-bit guard used by
   `market_config` and `special_user_status` (`admin.rs:4282-4287,4317-4322`) is missing here.
6. **`contract_tier` is unchecked and desynchronizes the insurance caps** (`admin.rs:2382`).
   A downgrade to any speculative tier leaves previously-set `quote_max_insurance` and
   `max_revenue_withdraw_per_period` above the new tier's cap (0) until the max-imbalances
   setter is re-run; nothing re-validates the pair.
7. **Perp `order_step_size` is not re-checked against existing state** (`admin.rs:2747`). A
   legal value that does not divide the market's aggregate base amounts halts all fills and
   PnL settlement (`validation/perp_market.rs:13-30` runs on every fill). `order_tick_size`
   has no upper cap and a large value kills post-only placement and AMM fills by underflow
   (`math/orders.rs:176,333,339`). `min_order_size` above u64::MAX/5000 takes funding down
   (`state/perp_market.rs:1005-1007`).
8. **`max_borrow_rate` has no absolute ceiling and applies retroactively** (u32 domain reaches
   ~429,496% APR; `validation/spot_market.rs:24-30` only orders it above the optimal rate).
   Because interest accrues over the whole un-accrued interval at the current curve
   (`math/spot_balance.rs:157-181`), one write can bill every existing borrower at an extreme
   rate for elapsed time.
9. **`insurance_fund.unstaking_period` is a fully unchecked signed value** (`admin.rs:1735`),
   read at unstake completion rather than latched at request time
   (`controller/insurance.rs:443-446`). Zero or negative releases every pending unstake
   instantly; a huge value strands them all.
10. **`liquidation_margin_buffer_ratio` is an uncapped u32** (`admin.rs:2569`). Large values
    make liquidations unable to exit and keep flagged users permanently blocked from new orders
    (`state/margin_calculation.rs:240-246`, `math/liquidation.rs:258-287`).
11. **`scale_initial_asset_weight_start` treats 1 and 0 very differently** (`admin.rs:2251`,
    unchecked): 0 disables the scaling, while a small nonzero value collapses the market's
    collateral contribution protocol-wide (`state/spot_market.rs:474-478`).
12. **`deposit_guard_threshold` lacks the $10k notional cap its withdraw-side mirror carries**
    (`admin.rs:2190` vs `validation/spot_market.rs:46-74`), so the deposit rate limiter can be
    disabled by one warm write.
13. **`fee_pool_buffer_target` is unchecked and silently stalls sweeps** (`admin.rs:2937`): an
    arbitrarily large value permanently zeroes the market's insurance-fund sweep and
    AMM-provision tokenization via saturating subtraction (`controller/perp_pools.rs:151-152`),
    and the setter uniquely lacks the Delisted-market gate.
14. **`SpotMarket.pool_id` can be repointed on a live market** (`admin.rs:462`): the
    status-must-be-Initialized gate is bypassable by the same warm admin via the unchecked
    status setter, and a repoint makes margin calculation fail for every user in the old pool
    (`math/margin.rs:289-297`).
15. **`cold_admin` accepts `Pubkey::default()`** (`admin.rs:3106`). That write permanently
    disables every cold-gated path, including the rotation handler itself.

Three cross-cutting caveats that are not single missing bounds:

- **Key collisions defeat the tier matrix.** A key that is both warm and pause admin bypasses
  the add-only rule and the perp pause bit budget (`auth.rs:118`, `admin.rs:2356`). The
  documented authority behavior holds only while the keys are distinct.
- **Doc-comment precision bugs invite 100x mis-sets.** `PerpMarket.imf_factor`,
  `PerpMarket.unrealized_pnl_imf_factor` (`state/perp_market.rs:380-385`) and
  `SpotMarket.imf_factor` (`state/spot_market.rs:148-150`) all claim `MARGIN_PRECISION` (1e4);
  the enforced bound and the consumers use `SPOT_IMF_PRECISION` (1e6). The
  `oracle_low_risk_slot_delay_override` doc comment (`state/perp_market.rs:451-453`) describes
  a mechanism that no longer exists. The SDK comment on `updateSpotMarketOrdersEnabled`
  (`packages/sdk/src/adminClient.ts:3776`) describes an effect the program does not implement.
- **"Unchecked" here means unchecked on-chain.** The admin CLI (`packages/cli-admin/`), the
  multisig policy, or deploy scripts may bound the same values; none of that was audited in
  this pass, and none of it binds a transaction signed directly by an admin key.

## Related documents

- [FEES.md](../FEES.md): the fee system end to end (waterfall, pools, insurance-fund
  economics). This document only covers the fee setters' bounds.
- [docs/EQUITY-FLOOR.md](./EQUITY-FLOOR.md): the per-user equity floor and breaker in full.
- [docs/DRIFT-TO-VELOCITY.md](./DRIFT-TO-VELOCITY.md): per-change integrator notes, including
  the removal of spot DLOB trading that makes several parameters in this document inert.
- `packages/cli-admin/README.md`: the operator command surface that drives these instructions.
