//! Rewrite a user's relay liquidation-condition block from their live
//! positions — the "which bucket is this user in" question the keeper bot
//! answers in memory, precomputed as thresholds instead.
//!
//! Permissionless and idempotent. This is the **opt-in** entry point: it
//! creates the account, so it takes a `payer: Signer`. Relay's
//! self-maintenance path is a separate instruction —
//! [`super::resync_liq_conditions`] — which names no signer at all,
//! because a staged executor is submitted unsigned (relay's turner marks
//! every meta non-signing and outright refuses to sign a transaction whose
//! executor names a signer). Both share [`rewrite_liq_conditions`].
//!
//! ## The threshold math
//!
//! Liquidatability is `maintenance_margin > total_collateral` over every
//! position and deposit at once. Locally, though, health is linear in each
//! individual price: moving one oracle by `Δp` moves free collateral by
//! roughly `−(∂margin/∂p − ∂collateral/∂p) · Δp`. So per exposure we solve
//! the ceteris-paribus price at which free collateral hits zero, and watch
//! *that* — with a haircut so it fires while buffer remains.
//!
//! Slopes (free collateral per unit of that market's raw price, carried in
//! BASE_PRECISION fixed-point so both exposure kinds share one distance
//! formula):
//!
//! - a perp position of size `b`: collateral moves `+b` (unrealized PnL),
//!   maintenance moves `+|b|·mmr`. Net free-collateral slope is
//!   `b − |b|·mmr` — long positions lose as price falls, shorts as it
//!   rises. `b` is already BASE_PRECISION-scaled.
//! - a spot deposit of `a` tokens with maintenance asset weight `w`:
//!   collateral moves `+a·w`, so free collateral falls as the price falls.
//!   (Spot *borrows* raise the requirement as their price rises; both
//!   directions are covered by the sign of the slope.) The token amount is
//!   scaled up to the same fixed-point.
//!
//! Because the estimate holds other prices fixed, a correlated move can
//! reach liquidatability before any single threshold trips — which is what
//! the haircut and the coarse fallback poll are for. Hints fire early or
//! bounded-late; the staged `liquidate_perp_with_fill` is the exact
//! predicate.

use {
    crate::{
        error::ErrorCode,
        math::{
            casting::Cast,
            constants::{MARGIN_PRECISION_U128, PRICE_PRECISION_I128, SPOT_WEIGHT_PRECISION_U128},
            safe_math::SafeMath,
            spot_balance::get_token_amount,
        },
        state::{
            clob_crank::ClobCrankConditionsV0,
            oracle::OracleSource,
            oracle_watch::{oracle_watch, OracleWatchV0, WatchDirection},
            pdas,
            perp_market::PerpMarket,
            prop_amm::QuoterV0,
            spot_market::{SpotBalanceType, SpotMarket},
            user::User,
            user_conditions::{
                LiqSlotMetaV0, UserConditionsV0, LIQ_SYNC_FALLBACK, LIQ_SYNC_WATCH,
                LIQ_THRESHOLD_SLOTS, USER_CONDITIONS_PDA_SEED,
            },
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0, ResolverListV0},
    std::{collections::BTreeMap, convert::TryInto},
};

/// Fraction of the distance-to-liquidation the threshold is pulled in by,
/// so a watch fires while there is still buffer: 20%. Absorbs the
/// single-oracle approximation, funding accrual, and price moves between
/// syncs; the cost of firing early is one free resolver simulation.
const THRESHOLD_HAIRCUT_BPS: i128 = 2_000;
const BPS_DENOM: i128 = 10_000;

/// The user's account region a sync watch covers: `perp_positions` through
/// `spot_positions`, i.e. everything whose change alters the thresholds.
/// A trade, deposit, or settlement lands here; unrelated writes (last
/// active slot, order bookkeeping) mostly do not.
pub fn user_positions_watch_region() -> (u32, u32) {
    let start = 8 + core::mem::offset_of!(User, spot_positions);
    let end = 8 + core::mem::offset_of!(User, orders);
    (start as u32, (end - start) as u32)
}

#[derive(Accounts)]
pub struct SyncLiqConditions<'info> {
    /// CHECK: in a manual sync this is whoever pays; when relay stages the
    /// sync it is the keeper payout target (paid from the conditions
    /// account's own lamports). Writable for both roles.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub user: AccountLoader<'info, User>,
    #[account(
        init_if_needed,
        seeds = [USER_CONDITIONS_PDA_SEED, user.key().as_ref()],
        space = UserConditionsV0::SIZE,
        bump,
        payer = payer
    )]
    pub liq_conditions: AccountLoader<'info, UserConditionsV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct SyncLiqConditionsArgs {
    /// Fee the staged self-sync pays its keeper, from this account's own
    /// lamports. 0 keeps the watch/poll conditions inactive (manual syncs
    /// only) — turners have no signal to take unpaid work.
    pub sync_payment_lamports: u64,
    /// Coarse fallback interval, in slots. 0 = use the previous value.
    pub sync_fallback_slots: u64,
}

/// One market's inputs, collected from `remaining_accounts`.
#[derive(Default, Clone, Copy)]
struct MarketInputs {
    oracle: Option<Pubkey>,
    /// Where a raw-price watch reads this market's oracle, when its source
    /// has a registered layout. `None` leaves the market keeper-only.
    watch: Option<OracleWatchV0>,
    oracle_source: Option<OracleSource>,
    /// Maintenance margin ratio (perp) / maintenance asset weight (spot).
    maintenance_ratio: u32,
    /// The same at the initial tier — what the force-cancel gate answers to,
    /// and always the earlier crossing of the two.
    initial_ratio: u32,
    /// The oracle price in `PRICE_PRECISION` — not the raw field, which is
    /// only the same thing on a six-decimal feed.
    price: i128,
    keeper_payment_lamports: Option<u64>,
    decimals: u32,
    cumulative_deposit_interest: u128,
}

pub fn handle_sync_liq_conditions<'c: 'info, 'info>(
    ctx: Context<'info, SyncLiqConditions<'info>>,
    args: SyncLiqConditionsArgs,
) -> Result<()> {
    // The opt-in caller pays rent and funds the reservoir; paying them a
    // sync fee out of the account they just funded would be a wash, so the
    // fee belongs to the relay path only.
    rewrite_liq_conditions(
        &ctx.accounts.liq_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        args,
    )
}

/// Recompute the block from the user's live positions. Shared by the
/// opt-in sync and relay's unsigned resync.
pub fn rewrite_liq_conditions<'info>(
    liq_conditions: &AccountLoader<'info, UserConditionsV0>,
    user_loader: &AccountLoader<'info, User>,
    remaining_accounts: &'info [AccountInfo<'info>],
    args: SyncLiqConditionsArgs,
) -> Result<()> {
    let user_key = user_loader.key();
    let mut perps: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut spots: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut oracle_of_market: BTreeMap<Pubkey, (bool, u16)> = BTreeMap::new();
    let mut market_refs: Vec<AccountRefV0> = Vec::new();
    let mut tail_refs: Vec<AccountRefV0> = Vec::new();
    let mut oracle_infos: BTreeMap<Pubkey, &AccountInfo<'info>> = BTreeMap::new();

    for info in remaining_accounts {
        if info.owner == &crate::ID {
            if let Ok(loader) = AccountLoader::<PerpMarket>::try_from(info) {
                let market = loader.load()?;
                let entry = perps.entry(market.market_index).or_default();
                entry.oracle = Some(market.oracle);
                entry.oracle_source = Some(market.oracle_source);
                entry.maintenance_ratio = market.margin_ratio_maintenance;
                entry.initial_ratio = market.margin_ratio_initial;
                oracle_of_market.insert(market.oracle, (true, market.market_index));
                market_refs.push(AccountRefV0::writable(info.key.to_bytes()));
                continue;
            }
            if let Ok(loader) = AccountLoader::<SpotMarket>::try_from(info) {
                let market = loader.load()?;
                let entry = spots.entry(market.market_index).or_default();
                entry.oracle = Some(market.oracle);
                entry.oracle_source = Some(market.oracle_source);
                entry.maintenance_ratio = market.maintenance_asset_weight;
                entry.initial_ratio = market.initial_asset_weight;
                entry.decimals = market.decimals;
                entry.cumulative_deposit_interest = market.cumulative_deposit_interest;
                oracle_of_market.insert(market.oracle, (false, market.market_index));
                market_refs.push(AccountRefV0::writable(info.key.to_bytes()));
                continue;
            }
            if let Ok(loader) = AccountLoader::<ClobCrankConditionsV0>::try_from(info) {
                let conditions = loader.load()?;
                perps
                    .entry(conditions.market_index)
                    .or_default()
                    .keeper_payment_lamports = Some(conditions.keeper_payment_lamports);
                tail_refs.push(AccountRefV0::readonly(info.key.to_bytes()));
                continue;
            }
            // Quoter entries are the trigger pass's input, not this one's —
            // but this pass writes the shared account list both passes'
            // staged executors reuse, so they must be kept out of the map
            // section: `load_maps` stops at the first non-map account, and
            // a quoter filed among the oracles cuts the markets off from
            // every staged executor (the localnet harness hit exactly that
            // as `PerpMarketNotFound`). Stored after the markets, where the
            // parser never reaches, so a staged resync still carries them.
            if let Ok(loader) = AccountLoader::<QuoterV0>::try_from(info) {
                let _ = loader.load()?;
                tail_refs.push(AccountRefV0::readonly(info.key.to_bytes()));
                continue;
            }
            // Anything else velocity-owned falls through with the oracle
            // candidates below — deliberately: velocity hosts its own
            // oracle accounts (PythLazer, prelaunch), and they must land
            // in the map section.
        }
        oracle_infos.insert(*info.key, info);
    }

    // Oracle prices (readonly, first in map order). The watch layout is
    // resolved off each oracle's bytes once, here, and carries both the
    // current price and where a threshold condition should read it.
    let mut oracle_refs: Vec<AccountRefV0> = Vec::new();
    for (key, info) in &oracle_infos {
        oracle_refs.push(AccountRefV0::readonly(key.to_bytes()));
        let Some((is_perp, market_index)) = oracle_of_market.get(key).copied() else {
            continue;
        };
        let entry = if is_perp {
            perps.entry(market_index).or_default()
        } else {
            spots.entry(market_index).or_default()
        };
        let Some(source) = entry.oracle_source else {
            continue;
        };
        let Some(watch) = oracle_watch(info, source) else {
            continue;
        };
        let Some(price) = watch.protocol_price() else {
            continue;
        };
        entry.watch = Some(watch);
        entry.price = price;
    }

    let conditions_key = liq_conditions.key();
    let disc8 = |disc: &[u8]| -> Result<[u8; 8]> {
        disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
    };

    // The account list staged executors reuse, in load_maps order.
    // The stored list is also the threshold conditions' indirect resolver
    // list, so it leads with the accounts `ResolveLiquidatePerpWithFill`
    // names, in its declaration order. `read_sync_accounts` skips them.
    let mut sync_accounts = vec![
        AccountRefV0::writable(pdas::relay_scratch().to_bytes()),
        AccountRefV0::readonly(conditions_key.to_bytes()),
        AccountRefV0::readonly(user_key.to_bytes()),
        AccountRefV0::readonly(pdas::state().to_bytes()),
    ];
    sync_accounts.extend(oracle_refs);
    sync_accounts.extend(market_refs);
    sync_accounts.extend(tail_refs);

    let mut conditions = liq_conditions
        .load_init()
        .or_else(|_| liq_conditions.load_mut())?;
    conditions.user = user_key;
    conditions.sync_payment_lamports = args.sync_payment_lamports;
    if args.sync_fallback_slots > 0 {
        conditions.sync_fallback_slots = args.sync_fallback_slots;
    }
    let fallback_slots = conditions.sync_fallback_slots.max(1);
    conditions.init_block()?;
    // What the threshold conditions point relay at (prefix + margin map).
    // Every condition on this account points at the one stored list.
    let resolvers = conditions.write_sync_accounts(&sync_accounts)?;

    // Free collateral now — the distance each threshold is measured from.
    let user = crate::load!(user_loader)?;
    // Stamped before the thresholds so a sync that legitimately writes none
    // still converges — the resolver compares digests, not slot contents.
    conditions.positions_digest = UserConditionsV0::digest_positions(&user);
    // The threshold is priced at whichever stage this account is in.
    //
    // Cancelling always comes before liquidating: `force_cancel_clob_orders`
    // answers to the initial requirement and liquidation to the maintenance
    // one, so anything liquidatable was already cancellable and the two are
    // stages of one ladder rather than separate watches. While the account
    // rests orders on a book, the earlier crossing is the one worth waking
    // at; once they are gone there is nothing to cancel and the watch belongs
    // back at the liquidation price.
    //
    // Nothing has to move it. The self-maintenance watch below covers
    // `[spot_positions, orders)`, which is where `open_orders` / `open_bids` /
    // `open_asks` live, so the cancel that empties the book and the placement
    // that refills it both re-run this sync and re-derive the stage.
    let tier = if user
        .perp_positions
        .iter()
        .any(|position| user.clob_resident_open_orders(position.market_index) > 0)
    {
        CollateralTier::Initial
    } else {
        CollateralTier::Maintenance
    };
    let (free_collateral, target_market) = estimate_free_collateral(&user, &perps, &spots, tier)?;

    let mut slot_index = 0usize;
    if free_collateral > 0 {
        if let Some(target_market_index) = target_market {
            // One threshold per exposure, each solved against its own price.
            for position in user.perp_positions.iter() {
                if slot_index >= LIQ_THRESHOLD_SLOTS {
                    break;
                }
                if position.base_asset_amount == 0 {
                    continue;
                }
                let Some(inputs) = perps.get(&position.market_index).copied() else {
                    continue;
                };
                // Slope in BASE_PRECISION fixed-point: free collateral
                // (QUOTE_PRECISION) moves `slope / BASE_PRECISION` per raw
                // price unit (PRICE_PRECISION).
                let base = position.base_asset_amount as i128;
                let slope = base.safe_sub(
                    base.abs()
                        .safe_mul(inputs.maintenance_ratio as i128)?
                        .safe_div(MARGIN_PRECISION_U128 as i128)?,
                )?;
                if let Some(condition) = threshold_condition(
                    &inputs,
                    slope,
                    free_collateral,
                    resolvers,
                    disc8(crate::instruction::ResolveLiquidatePerpWithFill::DISCRIMINATOR)?,
                )? {
                    conditions.set_condition(slot_index, &condition)?;
                    conditions.slots[slot_index] = LiqSlotMetaV0 {
                        target_market_index: position.market_index,
                        active: 1,
                        padding: [0; 1],
                    };
                    slot_index += 1;
                }
            }
            for position in user.spot_positions.iter() {
                if slot_index >= LIQ_THRESHOLD_SLOTS {
                    break;
                }
                // Quote collateral has no price risk; borrows and non-quote
                // deposits both move health with their oracle.
                if position.market_index == 0 || position.scaled_balance == 0 {
                    continue;
                }
                let Some(inputs) = spots.get(&position.market_index).copied() else {
                    continue;
                };
                let amount = get_token_amount(
                    position.scaled_balance.cast::<u128>()?,
                    &spot_market_stub(&inputs),
                    &position.balance_type,
                )?
                .cast::<i128>()?;
                // Collateral slope per unit price, in the same
                // BASE_PRECISION fixed-point as the perp slope: deposits
                // help, borrows hurt.
                let signed = match position.balance_type {
                    SpotBalanceType::Deposit => amount,
                    SpotBalanceType::Borrow => -amount,
                };
                let slope = signed
                    .safe_mul(crate::math::constants::BASE_PRECISION_I128)?
                    .safe_mul(inputs.maintenance_ratio as i128)?
                    .safe_div(SPOT_WEIGHT_PRECISION_U128 as i128)?
                    .safe_div(10i128.pow(inputs.decimals.min(18)))?;
                // A spot exposure's liquidation is still a perp liquidation
                // here (the with-fill flavor); target the user's largest
                // perp position.
                if let Some(condition) = threshold_condition(
                    &inputs,
                    slope,
                    free_collateral,
                    resolvers,
                    disc8(crate::instruction::ResolveLiquidatePerpWithFill::DISCRIMINATOR)?,
                )? {
                    conditions.set_condition(slot_index, &condition)?;
                    conditions.slots[slot_index] = LiqSlotMetaV0 {
                        target_market_index,
                        active: 1,
                        padding: [0; 1],
                    };
                    slot_index += 1;
                }
            }
        }
    }
    for index in slot_index..LIQ_THRESHOLD_SLOTS {
        conditions.set_condition(index, &relay_spec::bytemuck::Zeroable::zeroed())?;
        conditions.slots[index] = LiqSlotMetaV0::default();
    }

    // Self-maintenance: the user's own position bytes changing re-derives
    // the thresholds, and a coarse poll catches whatever that misses.
    if args.sync_payment_lamports > 0 {
        let sync_spec = CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc: disc8(crate::instruction::ResolveResyncLiqConditions::DISCRIMINATOR)?,
            min_payment: args.sync_payment_lamports,
        };
        let (watch_offset, watch_len) = user_positions_watch_region();
        conditions.set_condition(
            LIQ_SYNC_WATCH,
            &ConditionV0::on_account_change(
                user_key.to_bytes(),
                watch_offset,
                watch_len,
                sync_spec,
                resolvers,
            ),
        )?;
        conditions.set_condition(
            LIQ_SYNC_FALLBACK,
            &ConditionV0::every_slots(fallback_slots, sync_spec, resolvers),
        )?;
    } else {
        conditions.set_condition(LIQ_SYNC_WATCH, &relay_spec::bytemuck::Zeroable::zeroed())?;
        conditions.set_condition(LIQ_SYNC_FALLBACK, &relay_spec::bytemuck::Zeroable::zeroed())?;
    }
    Ok(())
}

/// A `SpotMarket` shaped just enough for `get_token_amount`.
fn spot_market_stub(inputs: &MarketInputs) -> SpotMarket {
    let mut market = SpotMarket::default();
    market.cumulative_deposit_interest = inputs.cumulative_deposit_interest;
    market.cumulative_borrow_interest = inputs.cumulative_deposit_interest;
    market.decimals = inputs.decimals;
    market
}

/// Free collateral (maintenance) approximated from the same linear model,
/// plus the user's largest perp position (the liquidation target for
/// spot-driven thresholds). Deliberately a local estimate: the executor
/// does the real calculation.
/// Which tier's ratios [`estimate_free_collateral`] values the account at.
///
/// `Maintenance` answers "when is this liquidatable"; `Initial` answers "when
/// does this stop being allowed to rest risk-increasing orders" — the gate
/// `force_cancel_clob_orders` gets its grounds from. Initial is the stricter
/// requirement, so its free collateral is always the smaller number and its
/// crossing is always the earlier price.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CollateralTier {
    Initial,
    Maintenance,
}

fn estimate_free_collateral(
    user: &User,
    perps: &BTreeMap<u16, MarketInputs>,
    spots: &BTreeMap<u16, MarketInputs>,
    tier: CollateralTier,
) -> Result<(i128, Option<u16>)> {
    let perp_ratio = |inputs: &MarketInputs| match tier {
        CollateralTier::Initial => inputs.initial_ratio,
        CollateralTier::Maintenance => inputs.maintenance_ratio,
    };
    let mut collateral: i128 = 0;
    let mut requirement: i128 = 0;
    let mut largest: Option<(u16, i128)> = None;

    for position in user.perp_positions.iter() {
        if position.base_asset_amount == 0 && position.quote_asset_amount == 0 {
            continue;
        }
        let Some(inputs) = perps.get(&position.market_index) else {
            continue;
        };
        let base = position.base_asset_amount as i128;
        let notional = base
            .abs()
            .safe_mul(inputs.price)?
            .safe_div(crate::math::constants::BASE_PRECISION_I128)?;
        collateral = collateral.safe_add(
            base.safe_mul(inputs.price)?
                .safe_div(crate::math::constants::BASE_PRECISION_I128)?
                .safe_add(position.quote_asset_amount as i128)?,
        )?;
        requirement = requirement.safe_add(
            notional
                .safe_mul(perp_ratio(inputs) as i128)?
                .safe_div(MARGIN_PRECISION_U128 as i128)?,
        )?;
        if base != 0 && largest.is_none_or(|(_, n)| notional > n) {
            largest = Some((position.market_index, notional));
        }
    }
    for position in user.spot_positions.iter() {
        if position.scaled_balance == 0 {
            continue;
        }
        let Some(inputs) = spots.get(&position.market_index) else {
            continue;
        };
        let amount = get_token_amount(
            position.scaled_balance as u128,
            &spot_market_stub(inputs),
            &position.balance_type,
        )?
        .cast::<i128>()?;
        let price = if position.market_index == 0 {
            crate::math::constants::PRICE_PRECISION_I128
        } else {
            inputs.price
        };
        let value = amount
            .safe_mul(price)?
            .safe_div(10i128.pow(inputs.decimals.clamp(1, 18)))?;
        match position.balance_type {
            SpotBalanceType::Deposit => {
                let weight = if position.market_index == 0 {
                    SPOT_WEIGHT_PRECISION_U128 as i128
                } else {
                    perp_ratio(inputs) as i128
                };
                collateral = collateral.safe_add(
                    value
                        .safe_mul(weight)?
                        .safe_div(SPOT_WEIGHT_PRECISION_U128 as i128)?,
                )?;
            }
            SpotBalanceType::Borrow => {
                collateral = collateral.safe_sub(value)?;
                requirement = requirement.safe_add(value)?;
            }
        }
    }
    Ok((collateral.safe_sub(requirement)?, largest.map(|(m, _)| m)))
}

/// Solve one exposure's ceteris-paribus liquidation price and turn it into
/// a haircut `OnValueCross` condition. `None` when the exposure can't be
/// watched (no lazer layout, zero slope, or a threshold off the price
/// axis) — those stay on the keeper-bot floor.
#[allow(clippy::too_many_arguments)]
fn threshold_condition(
    inputs: &MarketInputs,
    slope: i128,
    free_collateral: i128,
    resolvers: ResolverListV0,
    resolver_disc: [u8; 8],
) -> Result<Option<ConditionV0>> {
    let (Some(oracle), Some(watch), Some(min_payment)) =
        (inputs.oracle, inputs.watch, inputs.keeper_payment_lamports)
    else {
        return Ok(None);
    };
    if slope == 0 || inputs.price <= 0 {
        return Ok(None);
    }
    // Δprice (raw PRICE_PRECISION units) that exhausts free collateral,
    // haircut toward early. `slope` is BASE_PRECISION fixed-point (free
    // collateral per raw price unit), so the scale cancels here — using
    // PRICE_PRECISION instead put every perp threshold a thousandth of the
    // true distance from spot (a wake on every tick) and drove every spot
    // threshold off the price axis (never armed).
    let distance = free_collateral
        .safe_mul(crate::math::constants::BASE_PRECISION_I128)?
        .safe_div(slope.abs())?;
    let haircut = distance
        .safe_mul(BPS_DENOM.safe_sub(THRESHOLD_HAIRCUT_BPS)?)?
        .safe_div(BPS_DENOM)?;
    // Positive slope = health falls as the price falls (long perp, deposit).
    let (threshold_price, direction) = if slope > 0 {
        (inputs.price - haircut, WatchDirection::AtOrBelow)
    } else {
        (inputs.price + haircut, WatchDirection::AtOrAbove)
    };
    if threshold_price <= 0 {
        return Ok(None);
    }
    let Some(raw) = watch.raw_threshold(threshold_price, direction) else {
        return Ok(None);
    };

    // The resolver needs its three named accounts *and* the whole margin
    // map — more than a condition holds inline — so it reads the list that
    // `rewrite_liq_conditions` already stored on this very account.
    Ok(Some(ConditionV0::on_value_cross(
        oracle.to_bytes(),
        watch.price_offset,
        watch.price_len,
        // Signed: every registered watch layout stores its price as i64.
        relay_spec::WatchValue::Signed(raw),
        direction.cmp(),
        CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc,
            min_payment,
        },
        resolvers,
    )))
}

// Referenced by the doc comment's precision notes.
const _: () = {
    let _ = PRICE_PRECISION_I128;
};

pub fn validate_sync_args(args: &SyncLiqConditionsArgs) -> Result<()> {
    validate!(
        args.sync_fallback_slots > 0 || args.sync_payment_lamports == 0,
        ErrorCode::DefaultError,
        "a paid self-sync needs a fallback interval"
    )?;
    Ok(())
}
