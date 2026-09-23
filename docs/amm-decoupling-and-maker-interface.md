# AMM decoupling and maker interface

## Why

`PerpMarket.amm: AMM` began as one struct holding the vAMM's state. Over time it absorbed
everything adjacent to it, including position counters, fee accounting, funding cumulatives, oracle
metadata and order parameters. Before the refactor it carried roughly 80 fields, and the program read `.amm.` in
about 2,270 places, some 1,600 of them outside AMM-dedicated modules. Insurance, liquidation,
settlement, margin, funding and admin code all reached into `perp_market.amm.X` for state that has
nothing to do with AMM mechanics.

That arrangement blocks two things.

1. Replacing the vAMM, or running it alongside other makers. Before the refactor the vAMM was the
   only liquidity source `controller/orders.rs` knew how to talk to, and the matching logic was
   hard-coded for AMM-versus-DLOB-order pairs. Adding a parametric quoter, or running two AMMs on
   one market, would have meant an entirely new code path.

2. Extracting the AMM into its own onchain program. Every reader assumed `AMM` sat inline in the
   perp market account. Moving it to a separate account or program means rewriting every one of
   those readers.

The refactor, once it lands in full, gives the program four properties.

- `PerpMarket` is self-sufficient. Every consumer outside the AMM module reads and writes
  `PerpMarket` fields directly, and no code outside `programs/velocity/src/vlp/amm/` references AMM
  internals.
- The AMM is one `Quoter` implementation. DLOB resting orders are another. JIT auction participants
  are a third. The fill engine in `controller/matching.rs` walks them through the trait.
- The AMM struct splits into two explicit sub-structs, `AmmQuoteState` for the small fast-mutating
  part and `AmmBookkeeping` for the accounting layer, so the later excision cuts along a line that
  already exists.
- The `amm` field sits last in `PerpMarket`, so excising it does not disturb any other field offset.

## What has landed and what has not

The AMM now lives under `programs/velocity/src/vlp/amm/`, split into `state.rs` (the struct),
`quoter.rs` (the `Quoter` impls), `controller.rs` (swap application), `refresh.rs` (the curve
refresh), `admin.rs` and `math/` (`amm.rs`, `cp_curve.rs`, `jit.rs`, `repeg.rs`, `spread.rs`). The
`Quoter` trait lives in `programs/velocity/src/state/quoter.rs` and the fill engine in
`programs/velocity/src/controller/matching.rs`. `PerpMarket.market_stats: MarketStats` exists and
carries the migrated stats. The AMM struct is down to 38 fields
(`programs/velocity/src/vlp/amm/state.rs:80`).

Three pieces of the design are not in the code yet.

- The AMM struct is still flat. `AmmQuoteState` and `AmmBookkeeping` do not exist as types, and the
  fee-field split between the protocol portion and the AMM portion has not happened. That split
  changes behavior, not just field placement, so it is a separate change.
- `amm` is not the last field of `PerpMarket`. `hedge_config` and `_padding_future` follow it.
- Field visibility is not yet locked down to the AMM module, so reach-throughs still compile. About
  268 non-test lines outside `vlp/amm/` still write `perp_market.amm.X` or `market.amm.X`, and about
  1,102 lines including tests.

## Long-term architecture

The target is a shared orderbook that takes liquidity from `n` interchangeable makers: the vAMM,
DLOB resting orders, JIT auction participants, and whatever comes later, such as parametric curve
quoters or cross-program makers reached by CPI. Each maker is two things.

1. Some bytes of internal state, opaque to everyone outside the maker. The vAMM's bytes are
   reserves, peg, sqrt_k, spreads, AMM-private oracle snapshots and inventory. A DLOB-order maker's
   bytes are the resting `Order`. A future maker decides for itself what to store.

2. A quoting formula plus a fill-effect formula. Both are pure functions of `(self.bytes, ctx)`. The
   quoting formula computes `best_price`, `level_capacity` and the closed-form `try_fill_solo`. The
   fill-effect formula, `commit_fill`, defines how the maker's bytes change when a fill lands. `ctx`
   carries the inputs the fill engine shares across all makers, which are `MarketStats`, oracle
   data, the available fee budget, and the tick and step sizes.

The engine walks makers through this uniform interface and computes the blended fill. Settlement
happens through per-maker `commit_fill` after the engine decides who won which slice.

Eventually the vAMM lives in its own program. The orderbook program holds `AmmQuoteState` or its
equivalent next to whatever other makers' quote-state structs it tracks, and the AMM program holds
`AmmBookkeeping`. The orderbook emits fill deltas that the AMM program consumes to update its books.
This refactor structures AMM state along that division today, while everything still lives in one
program.

## Field partition

One rule decides where a field goes. `PerpMarket` should not know whether a trade was filled by the
AMM, a DLOB maker or a JIT participant. Anything tagged specifically to "this came from the AMM"
is AMM-internal. Anything that aggregates across all fills is protocol-level.

### Moved from AMM to PerpMarket

Position and open-interest counters aggregate across all positions and are not specific to the AMM
as counterparty. They are `base_asset_amount_long`, `base_asset_amount_short`, `quote_asset_amount`,
`quote_entry_amount_long`, `quote_entry_amount_short`, `quote_break_even_amount_long`,
`quote_break_even_amount_short`, `total_social_loss`, `max_open_interest`.

`base_asset_amount_with_amm` does not move. It is the AMM's own net counterparty position, used for
inventory-aware quoting, and it stays in the AMM struct. The pre-refactor code updated it from
`update_position_with_base_asset_amount` on every position delta, which was correct only because
every fill had the AMM as counterparty. Only AMM-side fills update it now, through
`QuoterCommit::commit_fill`. DLOB-to-DLOB matches do not, and should not.

Protocol fees are collected on every fill regardless of maker. `total_exchange_fee` (taker fees) and
`total_liquidation_fee` (routed to the insurance fund or the protocol) now live in
`PerpMarket.fee_ledger: FeeLedger` alongside `pending_protocol_fee`, `pending_if_fee`,
`amm_protocol_fees_received` and `pending_amm_provision`.

The AMM's own books stay with the AMM: `total_fee`, `total_mm_fee`, `total_fee_minus_distributions`,
`total_fee_withdrawn`, `net_revenue_since_last_funding`, `fee_pool`. The AMM is a self-contained
market maker with its own profit and loss accounting and its own token vault. The protocol learns
about AMM revenue through explicit AMM-side instructions, such as an admin withdrawal to the revenue
pool, and never by reading AMM bytes.

One behavioral change deserves calling out. Before the refactor, some of these "AMM" fee fields were
written on every fill, including DLOB-to-DLOB matches where the AMM was not a counterparty.
`total_fee` and `total_fee_minus_distributions` were touched from both the AMM-side and the DLOB-side
fill paths in `controller/orders.rs`. That was an artifact of the AMM-centric architecture, in which
"AMM fields" doubled as "protocol fields" because the AMM was the only counterparty model.

The target rule is that `QuoterCommit::commit_fill` is the only writer of the AMM's books, so they
move only when the AMM filled a slice of the take. Protocol-level fees collected on every trade go
to the `FeeLedger`. The AMM-specific portion, meaning its earned spread plus the part of the fee
pool it manages, stays with the AMM. Where the current code conflates the two under one name such as
`total_fee`, the split gives the protocol portion a `PerpMarket` field and leaves the AMM portion on
the AMM.

None of the following has landed yet.

- `AmmBookkeeping.total_fee` (i128) is incremented only by AMM-side fills. The same holds for
  `total_mm_fee`, `total_fee_minus_distributions`, `total_fee_withdrawn`,
  `net_revenue_since_last_funding` and `fee_pool`.
- A new protocol-level accumulator on `PerpMarket` captures today's "fee minus distributions" value
  written from non-AMM paths. The AMM-side equivalent, its own fee-minus-distributions figure used
  for the repeg budget, stays as `AmmBookkeeping.total_fee_minus_distributions`.
- A protocol-level `net_revenue_since_last_funding` on `PerpMarket` holds the rolling revenue
  window. The AMM keeps its own copy for `has_too_much_drawdown`
  (`programs/velocity/src/vlp/amm/state.rs:389`), and the fill controller writes the `PerpMarket`
  copy on every fill.

The split is mechanical at the write sites. AMM-side writes go through `commit_fill`, and DLOB-side
writes go to `PerpMarket` fields directly from the fill controller.

Funding state applies to all positions, so it moved to `PerpMarket`. That covers
`cumulative_funding_rate_long`, `cumulative_funding_rate_short`, `last_funding_rate`,
`last_funding_rate_long`, `last_funding_rate_short`, `last_funding_rate_ts`,
`net_unsettled_funding_pnl`. Three funding-adjacent values went to `MarketStats` instead, because
the AMM reads them while quoting: `last_24h_avg_funding_rate`, `funding_period` and
`last_funding_oracle_twap`.

Oracle identity is configuration, set at market creation and not updated per fill, so it moved to
`PerpMarket`. That covers `oracle: Pubkey`, `oracle_source: OracleSource`,
`oracle_slot_delay_override` and `oracle_low_risk_slot_delay_override`. Oracle data is fresh market state and belongs in
`MarketStats`, covered below.

Order parameters split by reader. `order_step_size` and `order_tick_size` sit on `PerpMarket`.
`min_order_size` sits in `MarketStats`, because the AMM reads it when computing fallback prices,
spread reserves and the `can_lower_k` check.

### Moved from PerpMarket to the AMM side

The hedge-pool fee-routing configuration moved into `PerpMarket.hedge_config: HedgeConfig`
(`programs/velocity/src/vlp/hedge/state.rs`), which holds `pool_id`, `status`, `paused_operations`,
`exchange_fee_exclusion_scalar` and `fee_transfer_scalar`. The scalars are AMM-fee-allocation
policy. `status` and `paused_operations` are market-level operational gates.

### `PerpMarket.market_stats: MarketStats`

`MarketStats` holds historic market data that any quoter would want. It is defined at
`programs/velocity/src/state/perp_market.rs:1746` and is 216 bytes.

```rust
pub struct MarketStats {
    // Mark TWAPs and volatility
    pub last_mark_price_twap: u64,
    pub last_mark_price_twap_5min: u64,
    pub last_mark_price_twap_ts: i64,
    pub last_bid_price_twap: u64,
    pub last_ask_price_twap: u64,
    pub mark_std: u64,
    pub oracle_std: u64,
    pub last_oracle_conf_pct: u64,
    // Volume, intensity, activity
    pub volume_24h: u64,
    pub long_intensity_volume: u64,
    pub short_intensity_volume: u64,
    pub last_trade_ts: i64,
    // Market-wide config the AMM reads while quoting (moved from PerpMarket)
    pub last_24h_avg_funding_rate: i64,
    pub funding_period: i64,
    pub min_order_size: u64,
    // MM oracle snapshot (native handler target)
    pub mm_oracle_price: i64,
    pub mm_oracle_slot: u64,
    pub mm_oracle_sequence_id: u64,
    // Oracle data (moved from AMM)
    pub last_oracle_normalised_price: i64,
    pub last_reference_price_offset: i32,
    pub last_oracle_valid: bool,
    pub padding: [u8; 3],
    pub last_funding_oracle_twap: i64,
    pub historical_oracle_data: HistoricalOracleData,
}
```

A field belongs in `MarketStats` if it must update on every market fill, whether that fill came from
the vAMM, the DLOB, the JIT path or a future maker. The AMM reads `&MarketStats` as input and never
writes to it. This is what makes the vAMM safe to call rarely in the future system, because nothing
that needs updating on every market event depends on the AMM being touched.

All `MarketStats` writers are methods on `MarketStats` itself, defined in `state/perp_market.rs`.
They are `update_mark_std`, `update_oracle_std`, `update_oracle_conf_pct`, `update_volume_24h`,
`update_mark_twap`, `update_mark_twap_with_amm_bid_ask`, `update_mark_twap_crank` and
`update_oracle_twap`. The TWAP writers take `&AMM` read-only when they need to derive an input such
as the reserve price, the AMM bid and ask, or `base_spread`, and otherwise mutate only `self`.
`controller/market_stats.rs` documents that contract and is reserved for stat writes fed from the
fill engine once the engine owns fill orchestration.

`last_reference_price_offset` caches the reference price offset written by the AMM refresh after a
successful repeg or k-update. `update_amm_quote_state` reads it to reproduce the legacy time-decayed
smoothing transition, which clamps the per-slot move when the freshly computed offset flips sign
against the cached value and `curve_update_intensity > 100`.

### Native handler offsets

`handle_update_mm_oracle_native` (`programs/velocity/src/instructions/admin.rs:3911`, native
dispatch opcode 0) writes the `mm_oracle_*` fields by byte offset rather than deserializing the
account. `handle_update_amm_spread_adjustment_native` does the same for `amm_spread_adjustment`. The
offsets are pinned by `native_instruction_offsets::amm_zero_copy_offsets` in
`programs/velocity/src/state/traits/tests.rs`. The table measures from the start of the account,
including the 8-byte discriminator.

| Field | Struct | Absolute offset |
| --- | --- | --- |
| `mm_oracle_price` | `MarketStats` | 800 |
| `mm_oracle_slot` | `MarketStats` | 808 |
| `mm_oracle_sequence_id` | `MarketStats` | 816 |
| `amm_spread_adjustment` | `AMM` | 1282 |

`PerpMarket::SIZE` is 1560 bytes, which is 1552 for the struct plus the 8-byte discriminator
(`programs/velocity/src/state/perp_market.rs:626`). `MarketStats` starts at struct offset 672 and
`AMM` at 896. `AMM` is 384 bytes and `MarketStats` is 216.

### Stays in the AMM: quote state

The reserves and the curve are `base_asset_reserve`, `quote_asset_reserve`, `sqrt_k`, `peg_multiplier`,
`concentration_coef`, `min_base_asset_reserve`, `max_base_asset_reserve`,
`terminal_quote_asset_reserve`, `ask_base_asset_reserve`, `ask_quote_asset_reserve`,
`bid_base_asset_reserve`, `bid_quote_asset_reserve`.

The spread and behavior fields are `base_spread`, `max_spread`, `long_spread`, `short_spread`,
`amm_spread_adjustment`, `amm_inventory_spread_adjustment`, `amm_jit_intensity`,
`curve_update_intensity`, `reference_price_offset`, `reference_price_offset_deadband_pct`,
`max_fill_reserve_fraction`, `max_slippage_ratio`, `funding_bias_sensitivity`.

`base_asset_amount_with_amm` holds the AMM's own counterparty position, which drives
inventory-aware quoting.

The AMM-private oracle and funding snapshots are `last_oracle_reserve_price_spread_pct`,
`last_update_slot`, `last_spread_update_slot`, `last_cumulative_funding_rate_long` and
`last_cumulative_funding_rate_short`. These depend on AMM-specific reserves, tolerate staleness, and
refresh when the AMM is touched. `last_oracle_normalised_price` is not AMM-private despite its old
location. It is the canonical sanitised oracle reading that any quoter would want, so it moved to
`MarketStats`.

### Stays in the AMM: bookkeeping

`fee_pool: PoolBalance`, `total_fee`, `total_mm_fee`, `total_fee_minus_distributions`,
`total_fee_withdrawn`, `net_revenue_since_last_funding`.

The fill engine never reads the AMM's bookkeeping to produce a quote.
`total_fee_minus_distributions` feeds repeg budgets through `ctx.fee_budget`, but that is a scalar
the fill controller reads off bookkeeping and passes in. Quoting never touches the books.

### Dead LP padding, reclaimed

User-direct vAMM LP was removed in earlier commits (`e1e22230b`, `e1c92f578`, `7435cddb3`). Seven
AMM padding fields and three `PerpPosition` padding fields were left as dead bytes. Because devnet
uses wipe-and-reinit, that padding was removed outright rather than preserved.

## The `Quoter` interface

`programs/velocity/src/state/quoter.rs` defines the trait every liquidity source implements. The
module doc comment carries the architectural narrative and the trait doc carries the fill-algorithm
specification.

```rust
pub struct QuoteContext<'a> {
    pub stats: &'a MarketStats,
    pub oracle: &'a OraclePriceData,
    pub mm_oracle: Option<&'a MMOraclePriceData>,   // AMM setup only
    pub oracle_validity: Option<OracleValidity>,    // AMM setup only
    pub fee_budget: u64,
    pub tick: u64,
    pub step_size: u64,
    pub slot: u64,
    pub slot_clock: SlotClock,
    pub base_precision: u64,
    pub market_status: MarketStatus,                // AMM only
    pub market_config: u8,                          // AMM only
}

pub struct QuoterFill {
    pub side: PositionDirection,
    pub base_filled: u64,
    pub quote_filled: u64,
    pub clearing_price: u64,
    pub refresh_cost: u64,
    pub is_fee_exempt: bool,
    pub fee_policy: FillFeePolicy,
    pub quote_asset_amount_surplus: i64,
}

pub trait Quoter {
    fn setup(&mut self, _ctx: &QuoteContext) -> VelocityResult<()> { Ok(()) }

    /// The maker's single quoted price on this side. Returns the no-quote
    /// sentinel otherwise: u64::MAX for Long, 0 for Short.
    fn best_price(&self, ctx: &QuoteContext, side: PositionDirection) -> VelocityResult<u64>;

    /// Full fillable base at `best_price`. Cheap and non-mutating, computed
    /// analytically, never by running the actual fill. The discrete walk sizes
    /// each level with it.
    fn level_capacity(&self, ctx: &QuoteContext, side: PositionDirection) -> VelocityResult<u64>;

    fn is_prio(&self) -> bool { false }
    fn is_fee_exempt(&self) -> bool { false }
    fn fee_policy(&self) -> FillFeePolicy { FillFeePolicy::DlobMatch }

    /// Closed-form fill of `target_size` base at this maker's price.
    fn try_fill_solo(
        &self,
        ctx: &QuoteContext,
        side: PositionDirection,
        target_size: u64,
    ) -> VelocityResult<Option<QuoterFill>> {
        Ok(None)
    }
}

pub trait QuoterCommit: Quoter {
    fn commit_fill(&mut self, ctx: &QuoteContext, fill: &QuoterFill) -> VelocityResult<()>;
    fn on_market_event(&mut self, _ctx: &QuoteContext, _event: &MarketEvent) -> VelocityResult<()> { Ok(()) }
}
```

Quote methods are pure functions of `(self, ctx)`. `commit_fill` is the only mutation hook, so the
maker stays the sole authority on how its bytes change. `on_market_event` is the second mutation
channel, carrying market-level signals such as a funding application. The AMM uses it to fold its
formulaic k-update inside the maker boundary, which replaces the old pattern of the funding
controller writing into `market.amm`.

Three implementations exist today.

- `DlobOrderQuoter<'a>` (`state/quoter.rs:394`) wraps a single resting `Order`. It handles both
  resting DLOB orders and in-auction JIT participants through `Order::get_limit_price`, so the
  auction and oracle-offset pricing rules apply without a second code path. Discrete, single level.
- `AmmQuoter<'a>` (`vlp/amm/quoter.rs:237`) is the continuous constant-product curve, matched solo
  through `fill_amm_only`. It reports `is_prio = true`, `is_fee_exempt = true` and
  `fee_policy = AmmHouse`.
- `AmmJitQuoter<'a>` (`vlp/amm/quoter.rs:776`) is the AMM in JIT-making mode, exposed as a discrete
  single-price level. `best_price` is `jit_price` and `level_capacity` is
  `min(max_jit_base, reserve-bounded max)`. It reports the same three flags as `AmmQuoter`.

> Note on the continuous AMM. The constant-product vAMM is the one continuous maker. It is matched
> solo through `fill_amm_only`, which uses the inherent method `AmmQuoter::cumulative_size`
> (`vlp/amm/quoter.rs:286`), the analytical inverse of its curve, to cap a take at the taker's
> limit. `cumulative_size` is not on the `Quoter` trait, because the discrete level walk never needs
> it. It is built on `math::spread::calculate_base_asset_amount_to_trade_to_price`
> (`vlp/amm/math/spread.rs:1486`). When the vAMM participates alongside DLOB orders it does so as
> `AmmJitQuoter`, which quotes a single fixed `jit_price` and is therefore discrete, like a DLOB
> order.

`level_capacity` reports the full base a maker can fill at its level. It must be cheap and
non-mutating, so compute it analytically. For a DLOB order that is the remaining size. For the JIT
vAMM it is `min(throttle, reserve-bounded max)`. Probing capacity by swapping the AMM-JIT to its
reserve boundary errors out, so never run the swap.

`is_prio` marks makers that take their full level capacity at the clearing price before pro-rata
distributes the remainder to non-priority makers. Priority does not override price priority, so a
better `best_price` still wins regardless of `is_prio`. Both AMM quoters are priority makers and
DLOB orders are not.

`is_fee_exempt` marks makers that neither pay nor receive maker fees, because the vAMM earns from
the spread rather than from rebates. `fee_policy` selects the fill controller's fee path, which is
`AmmHouse` for AMM-side fills and `DlobMatch` for DLOB fills. Both flags are copied into each
`QuoterFill`.

`try_fill_solo` computes a maker's closed-form fill at its price. `fill_amm_only` calls it to fill
the whole take, and the discrete walk calls it once per winning maker on that maker's allocated
base. Every `Quoter` in the crate implements it.

## Fill algorithm

`controller/matching.rs` has two explicit fill paths. There is no general continuous-curve clearing
algorithm, no bisection and no price-domain search. The split reflects the fact that the only
continuous maker, the constant-product vAMM, is always matched solo, while every multi-maker fill
runs over discrete single-price makers.

### Path 1, `fill_amm_only` for the sole continuous vAMM

```text
fill_amm_only(amm, ctx, side, T, taker_limit):
    if amm.best_price(side) is no-quote or worse than taker_limit: empty
    cap = amm.cumulative_size(side, taker_limit)   # analytic curve inverse, clamps to the limit
    fill = amm.try_fill_solo(side, min(T, cap))    # closed-form swap
    amm.commit_fill(fill)
    return fill   # clearing_price = None on a partial
```

This is the only path that touches the curve. `cumulative_size` is an inherent `AmmQuoter` method,
called once rather than in a loop, and it exists purely to cap the take at the taker's limit. The
result is byte-exact with the legacy `swap_base_asset`.

### Path 2, `match_take` for the discrete level walk

```text
match_take(makers, ctx, side, T, taker_limit):
    # Each maker is one level: best_price + level_capacity (cheap, no swap).
    levels = [(id, best_price, is_prio, level_capacity)
              for each maker quoting on side within taker_limit, capacity > 0]
    sort levels best-first (price; prio wins ties)

    cumulative = 0
    for tie_group in levels grouped by equal price:
        group_supply = sum of capacity in tie_group
        if cumulative + group_supply <= T:
            fill each member its full capacity; cumulative += group_supply
            if cumulative == T: clearing_price = group.price; break
        else:
            # clearing level: distribute the residual across the tie group
            residual = T - cumulative
            prio members take full capacity first;
            non-prio split the rest pro-rata by capacity (last absorbs rounding)
            clearing_price = group.price; break
    # fell through without clearing => partial fill, clearing_price = None

    # commit once per winning maker against its TOTAL allocated base
    for maker with base > 0:
        fill = maker.try_fill_solo(side, base)   # quote computed once, not per slice
        maker.commit_fill(fill)
```

No bisection, no tick-walking, no `cumulative_size`. The sort is stable, so price ties keep the
makers' original order and pro-rata stays deterministic. The quote is computed once per maker on its
total base rather than summed per slice, so the reported `quote_filled` equals what `commit_fill`
actually moved. That matters for the JIT vAMM, whose underlying swap is non-linear.

The cost is one `best_price` plus one `level_capacity` per maker, plus one `try_fill_solo` and one
`commit_fill` per winning maker. For the dominant sole-AMM case handled by `fill_amm_only` it is a
single analytical fill.

`match_take` allocates a `Vec` for the level list and another for the per-maker base scratch. For
the common case of two or fewer makers a `SmallVec<[T; 4]>` would use fewer compute units. Profile
before changing it.

### AMM-side details

`fill_amm_only` runs on `AmmQuoter`. Its `Quoter::setup`, called by the orchestrator before the
fill, folds the conditional repeg and k-update into the AMM through `project_post_refresh_scalar`
(`vlp/amm/math/repeg.rs:463`) and refreshes the cached spread state through `update_amm_quote_state`.
`setup` is slot-idempotent, so re-running it in the same slot leaves the curve unchanged.
`commit_fill` applies the swap to the reserves, updates `base_asset_amount_with_amm` and re-derives
the cached ask and bid spread reserves through `refresh_cached_spread_reserves`
(`vlp/amm/math/spread.rs:305`). It reports `refresh_cost` on the `QuoterFill` for the fill
controller to apply to `PerpMarket`.

Only the AMM produces a non-zero `refresh_cost` today, from a repeg or k-update during `setup`.
Discrete makers report zero. Both `fill_amm_only` and the commit loop in `match_take` sum it into
`Match.total_refresh_cost`.

`calculate_base_swap_output` (`vlp/amm/controller.rs:93`) returns a named struct rather than a
tuple: `AmmSwapOutput { new_base_asset_reserve, new_quote_asset_reserve, quote_asset_amount,
quote_asset_amount_surplus }`.

`AmmJitQuoter` participates in `match_take` as a discrete level. Its `commit_fill` moves reserves
along the curve while the taker pays `jit_price`, and the gap between the two is
`quote_asset_amount_surplus`, which goes negative when the AMM subsidises the fill.

`controller::matching::fill_perp_market_against_amm` (`controller/matching.rs:389`) wraps
`fill_amm_only` for sole-AMM fills and replaces the legacy `swap_base_asset` plus manual bookkeeping.
The post-fill AMM bookkeeping lives in `AmmQuoter::commit_fill`, so no separate post-match wrapper
is needed.

### Snapshot consistency

`best_price`, `level_capacity` and `try_fill_solo` must be pure functions of `(self, ctx)`. They
perform no mutation, no global side effects, and no clock reads that are not already in `ctx`.
`commit_fill` is the only place state changes, and it runs after the fill resolves.

## Fill paths after the refactor

- Pure AMM fill, covering settlement, liquidation and the keeper or AMM order path, runs
  `fill_amm_only(amm_quoter, ctx, side, T, limit)`. This is the dominant production case.
- AMM JIT alongside a DLOB cross runs
  `match_take(&mut [&mut amm_jit, &mut dlob_quoter], ctx, side, T, limit)`. Both are discrete
  single-price levels, and the walk routes by price, with the priority JIT vAMM winning ties.
- DLOB-only runs `match_take` over the DLOB order levels, with the AMM-JIT contributing zero when it
  declines. Today that is the AMM-JIT-declines case. Later it is spline levels.
- Future spline liquidity arrives as compact spline regions pushed by off-chain market makers, which
  the program materialises into discrete levels through a `SplineQuoter` that feeds the same
  `match_take` walk. At that point `fill_amm_only` and the constant-product math can be removed along
  with the vAMM, leaving the discrete walk as the whole engine.

After the match, the fill controller applies the aggregate `Match`: position counters on
`PerpMarket`, protocol fees driven by each maker's `is_fee_exempt` flag, the pnl pool, social loss,
funding state when a funding update is due, `MarketStats` updates, and `total_refresh_cost` deducted
from AMM bookkeeping through the AMM module's helper.

The hard-coded pairwise matching rules and the DLOB-fill and JIT-auction logic in
`controller/orders.rs` can then be removed.

## Boundary enforcement

AMM field visibility is scoped to `programs/velocity/src/vlp/amm/` and its submodules. Trait impls
and `commit_fill` helpers live inside that tree. External callers reach AMM state only through the
`Quoter` interface, or through AMM-defined methods on `&PerpMarket` for AMM-specific operations that
do not fit the trait, such as admin withdrawals from `fee_pool` to the revenue pool and
operator-forced repegs.

Three checks say the boundary holds.

- `rg 'perp_market\.amm\.|market\.amm\.' programs/velocity/src/ -g '!vlp/amm/**'` returns nothing.
  It currently returns about 1,102 lines, 268 of them outside tests.
- `rg 'PerpMarket' programs/velocity/src/vlp/amm/` matches nothing in function signatures, only in
  `use` imports.
- An external module writing `market.amm.total_fee`, or any other AMM field, fails to compile.

## Out of scope

- Cross-program `Quoter`, meaning CPI into maker programs. For now the trait is in-program Rust
  polymorphism. The cross-program form needs serialized-curve returns plus read-only CPI, so design
  it when it is needed.
- Tolerance-band pro-rata, meaning distribution across makers whose prices are within epsilon of
  each other rather than exactly tied. The engine ships with strict price priority and pro-rata only
  at exactly-tied ticks. Tolerance bands are a future policy knob.
- Cross-program AMM excision itself. This refactor structures the code along the future division but
  keeps everything in `programs/velocity`. Moving to a separate AMM program is a follow-up.
- Parametric-curve quoters, meaning spline liquidity. These land as additional `Quoter` impls
  without changes to the engine.

## File layout

```
docs/
  amm-decoupling-and-maker-interface.md      this file
  alignment-and-native-offsets.md            zero-copy layout invariants

programs/velocity/src/
  state/
    perp_market.rs       PerpMarket, FeeLedger, MarketStats
    quoter.rs            Quoter, QuoteContext, QuoterFill, QuoterCommit,
                         FillFeePolicy, MarketEvent, DlobOrderQuoter
    traits/tests.rs      native-handler offset regression tests
  controller/
    matching.rs          fill engine: fill_amm_only, match_take, Match,
                         fill_perp_market_against_amm
    market_stats.rs      documented landing place for engine-fed stat writes
    perp_pools.rs        market-level pool accounting: sweep_market_fees,
                         update_pool_balances, update_pnl_pool_and_user_balance,
                         get_pnl_pool_drain_reserve_price
    orders.rs            DLOB fill and JIT auction logic
    funding.rs           reads and writes PerpMarket funding fields
    pnl.rs               reads PerpMarket position counters
  math/
    perp_market.rs       calculate_perp_market_amm_summary_stats
  vlp/amm/
    state.rs             the AMM struct
    quoter.rs            AmmQuoter, AmmJitQuoter, their Quoter/QuoterCommit impls
    controller.rs        swap application, AmmSwapOutput
    refresh.rs           curve refresh applied during Quoter::setup
    admin.rs             AMM admin instructions
    math/                amm.rs, cp_curve.rs, jit.rs, repeg.rs, spread.rs
  instructions/
    admin.rs             native handlers reading MarketStats and AMM by offset
```

## Verification

All of the following must hold before a change to this area merges.

- `bun run fmt:rust && cargo clippy -p velocity` is clean.
- `cargo test -p velocity` passes the full unit suite. Give particular scrutiny to `size`,
  `market_index_offset`, `native_instruction_offsets`, the engine property tests and the AMM-JIT
  differential tests.
- `bash test-scripts/run-anchor-tests.sh` passes the full integration suite.
- `cd packages/sdk && bun run prettify && bun run lint && bun run test:ci && bun run test:dlob` is
  clean.
- Compute-unit benchmarks on representative fills (pure AMM, AMM JIT, JIT auction with residual)
  land within about 10% of the pre-change baseline.
- The boundary checks above do not regress.

After merging a layout-breaking change, run the devnet wipe-and-reinit described in the root
`CLAUDE.md` runbook.
