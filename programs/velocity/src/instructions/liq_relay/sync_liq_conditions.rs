//! Rewrite a user's relay liquidation-condition block from their live
//! positions — the "which bucket is this user in" question the keeper bot
//! answers in memory, precomputed as thresholds instead.
//!
//! Permissionless and idempotent. Two callers matter: whoever opts a user
//! in (paying rent + funding the sync reservoir), and relay itself — the
//! block's own sync-watch condition stages *this instruction* whenever the
//! user's positions change, so the hints maintain themselves and the
//! keeper is paid from the account's own lamports.
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
//! Slopes (per unit of that market's price, in PRICE_PRECISION):
//!
//! - a perp position of size `b`: collateral moves `+b` (unrealized PnL),
//!   maintenance moves `+|b|·mmr`. Net free-collateral slope is
//!   `b − |b|·mmr` — long positions lose as price falls, shorts as it
//!   rises.
//! - a spot deposit of `a` tokens with maintenance asset weight `w`:
//!   collateral moves `+a·w`, so free collateral falls as the price falls.
//!   (Spot *borrows* raise the requirement as their price rises; both
//!   directions are covered by the sign of the slope.)
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
            liq_conditions::{
                LiqConditionsV0, LiqSlotMetaV0, LIQ_CONDITIONS_PDA_SEED, LIQ_SYNC_FALLBACK,
                LIQ_SYNC_WATCH, LIQ_THRESHOLD_SLOTS,
            },
            oracle::OracleSource,
            perp_market::PerpMarket,
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::{SpotBalanceType, SpotMarket},
            user::User,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::{collections::BTreeMap, convert::TryInto},
};

/// PythLazer raw-price watch layout (see `sync_trigger_conditions`).
const PYTH_LAZER_PRICE_OFFSET: u32 = 8;
const PYTH_LAZER_PRICE_LEN: u32 = 8;
const PYTH_LAZER_EXPONENT_OFFSET: usize = 32;

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
        seeds = [LIQ_CONDITIONS_PDA_SEED, user.key().as_ref()],
        space = LiqConditionsV0::SIZE,
        bump,
        payer = payer
    )]
    pub liq_conditions: AccountLoader<'info, LiqConditionsV0>,
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
    is_lazer: bool,
    exponent: Option<i32>,
    /// Maintenance margin ratio (perp) / maintenance asset weight (spot).
    maintenance_ratio: u32,
    price: i64,
    keeper_payment_lamports: Option<u64>,
    decimals: u32,
    cumulative_deposit_interest: u128,
}

pub fn handle_sync_liq_conditions<'c: 'info, 'info>(
    ctx: Context<'info, SyncLiqConditions<'info>>,
    args: SyncLiqConditionsArgs,
) -> Result<()> {
    let mut perps: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut spots: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut oracle_of_market: BTreeMap<Pubkey, (bool, u16)> = BTreeMap::new();
    let mut market_refs: Vec<AccountRefV0> = Vec::new();
    let mut tail_refs: Vec<AccountRefV0> = Vec::new();
    let mut oracle_infos: BTreeMap<Pubkey, &AccountInfo<'info>> = BTreeMap::new();

    for info in ctx.remaining_accounts {
        if info.owner == &crate::ID {
            if let Ok(loader) = AccountLoader::<PerpMarket>::try_from(info) {
                let market = loader.load()?;
                let entry = perps.entry(market.market_index).or_default();
                entry.oracle = Some(market.oracle);
                entry.is_lazer = matches!(market.oracle_source, OracleSource::PythLazer);
                entry.maintenance_ratio = market.margin_ratio_maintenance;
                oracle_of_market.insert(market.oracle, (true, market.market_index));
                market_refs.push(AccountRefV0::writable(info.key.to_bytes()));
                continue;
            }
            if let Ok(loader) = AccountLoader::<SpotMarket>::try_from(info) {
                let market = loader.load()?;
                let entry = spots.entry(market.market_index).or_default();
                entry.oracle = Some(market.oracle);
                entry.is_lazer = matches!(market.oracle_source, OracleSource::PythLazer);
                entry.maintenance_ratio = market.maintenance_asset_weight;
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
        }
        oracle_infos.insert(*info.key, info);
    }

    // Oracle prices + exponents (readonly, first in map order).
    let mut oracle_refs: Vec<AccountRefV0> = Vec::new();
    for (key, info) in &oracle_infos {
        oracle_refs.push(AccountRefV0::readonly(key.to_bytes()));
        let Some((is_perp, market_index)) = oracle_of_market.get(key).copied() else {
            continue;
        };
        let data = info.try_borrow_data()?;
        if data.len() < 8 + core::mem::size_of::<PythLazerOracle>()
            || &data[..8] != PythLazerOracle::DISCRIMINATOR
        {
            continue;
        }
        let price = i64::from_le_bytes(
            data[8..16]
                .try_into()
                .map_err(|_| ErrorCode::DefaultError)?,
        );
        let exponent = i32::from_le_bytes(
            data[PYTH_LAZER_EXPONENT_OFFSET..PYTH_LAZER_EXPONENT_OFFSET + 4]
                .try_into()
                .map_err(|_| ErrorCode::DefaultError)?,
        );
        let entry = if is_perp {
            perps.entry(market_index).or_default()
        } else {
            spots.entry(market_index).or_default()
        };
        entry.exponent = Some(exponent);
        entry.price = price;
    }

    let user_key = ctx.accounts.user.key();
    let conditions_key = ctx.accounts.liq_conditions.key();
    let disc8 = |disc: &[u8]| -> Result<[u8; 8]> {
        disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
    };

    // The account list staged executors reuse, in load_maps order.
    let mut sync_accounts = oracle_refs;
    sync_accounts.extend(market_refs);
    sync_accounts.extend(tail_refs);

    let mut conditions = ctx
        .accounts
        .liq_conditions
        .load_init()
        .or_else(|_| ctx.accounts.liq_conditions.load_mut())?;
    conditions.user = user_key;
    conditions.sync_payment_lamports = args.sync_payment_lamports;
    if args.sync_fallback_slots > 0 {
        conditions.sync_fallback_slots = args.sync_fallback_slots;
    }
    let fallback_slots = conditions.sync_fallback_slots.max(1);
    conditions.init_header()?;
    conditions.write_sync_accounts(&sync_accounts)?;

    // Free collateral now — the distance each threshold is measured from.
    let user = crate::load!(ctx.accounts.user)?;
    let (free_collateral, target_market) = estimate_free_collateral(&user, &perps, &spots)?;

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
                    &conditions_key,
                    &user_key,
                    position.market_index,
                    disc8(crate::instruction::ResolveLiquidatePerpWithFill::DISCRIMINATOR)?,
                    disc8(crate::instruction::LiquidatePerpWithFill::DISCRIMINATOR)?,
                )? {
                    conditions.write_condition(slot_index, &condition)?;
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
                // Collateral slope per unit price: deposits help, borrows hurt.
                let signed = match position.balance_type {
                    SpotBalanceType::Deposit => amount,
                    SpotBalanceType::Borrow => -amount,
                };
                let slope = signed
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
                    &conditions_key,
                    &user_key,
                    target_market_index,
                    disc8(crate::instruction::ResolveLiquidatePerpWithFill::DISCRIMINATOR)?,
                    disc8(crate::instruction::LiquidatePerpWithFill::DISCRIMINATOR)?,
                )? {
                    conditions.write_condition(slot_index, &condition)?;
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
        conditions.write_condition(index, &relay_spec::bytemuck::Zeroable::zeroed())?;
        conditions.slots[index] = LiqSlotMetaV0::default();
    }

    // Self-maintenance: the user's own position bytes changing re-derives
    // the thresholds, and a coarse poll catches whatever that misses.
    if args.sync_payment_lamports > 0 {
        let sync_spec = CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc: disc8(crate::instruction::ResolveSyncLiqConditions::DISCRIMINATOR)?,
            executor_program: crate::ID.to_bytes(),
            executor_disc: disc8(crate::instruction::SyncLiqConditions::DISCRIMINATOR)?,
            min_payment: args.sync_payment_lamports,
        };
        let sync_resolver_accounts = [
            AccountRefV0::writable(conditions_key.to_bytes()),
            AccountRefV0::readonly(user_key.to_bytes()),
        ];
        let (watch_offset, watch_len) = user_positions_watch_region();
        conditions.write_condition(
            LIQ_SYNC_WATCH,
            &ConditionV0::on_account_change(
                user_key.to_bytes(),
                watch_offset,
                watch_len,
                sync_spec,
                &sync_resolver_accounts,
            ),
        )?;
        conditions.write_condition(
            LIQ_SYNC_FALLBACK,
            &ConditionV0::every_slots(fallback_slots, sync_spec, &sync_resolver_accounts),
        )?;
    } else {
        conditions.write_condition(LIQ_SYNC_WATCH, &relay_spec::bytemuck::Zeroable::zeroed())?;
        conditions.write_condition(LIQ_SYNC_FALLBACK, &relay_spec::bytemuck::Zeroable::zeroed())?;
    }
    drop(conditions);

    // When relay staged this sync, the payer is the keeper: pay them from
    // the account's own lamports (best-effort — a manual sync must land
    // even on an empty reservoir).
    if args.sync_payment_lamports > 0 {
        let info = ctx.accounts.liq_conditions.to_account_info();
        let rent_minimum = Rent::get()?.minimum_balance(info.data_len());
        LiqConditionsV0::pay_sync_keeper(
            &info,
            &ctx.accounts.payer.to_account_info(),
            args.sync_payment_lamports,
            rent_minimum,
        )?;
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
fn estimate_free_collateral(
    user: &User,
    perps: &BTreeMap<u16, MarketInputs>,
    spots: &BTreeMap<u16, MarketInputs>,
) -> Result<(i128, Option<u16>)> {
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
            .safe_mul(inputs.price as i128)?
            .safe_div(crate::math::constants::BASE_PRECISION_I128)?;
        collateral = collateral.safe_add(
            base.safe_mul(inputs.price as i128)?
                .safe_div(crate::math::constants::BASE_PRECISION_I128)?
                .safe_add(position.quote_asset_amount as i128)?,
        )?;
        requirement = requirement.safe_add(
            notional
                .safe_mul(inputs.maintenance_ratio as i128)?
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
            inputs.price as i128
        };
        let value = amount
            .safe_mul(price)?
            .safe_div(10i128.pow(inputs.decimals.clamp(1, 18)))?;
        match position.balance_type {
            SpotBalanceType::Deposit => {
                let weight = if position.market_index == 0 {
                    SPOT_WEIGHT_PRECISION_U128 as i128
                } else {
                    inputs.maintenance_ratio as i128
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
    conditions_key: &Pubkey,
    user_key: &Pubkey,
    target_market_index: u16,
    resolver_disc: [u8; 8],
    executor_disc: [u8; 8],
) -> Result<Option<ConditionV0>> {
    let (Some(oracle), Some(exponent), Some(min_payment)) = (
        inputs.oracle,
        inputs.exponent,
        inputs.keeper_payment_lamports,
    ) else {
        return Ok(None);
    };
    if !inputs.is_lazer || slope == 0 || inputs.price <= 0 {
        return Ok(None);
    }
    // Δprice that exhausts free collateral, haircut toward early.
    let distance = free_collateral
        .safe_mul(crate::math::constants::PRICE_PRECISION_I128)?
        .safe_div(slope.abs())?;
    let haircut = distance
        .safe_mul(BPS_DENOM.safe_sub(THRESHOLD_HAIRCUT_BPS)?)?
        .safe_div(BPS_DENOM)?;
    // Positive slope = health falls as the price falls (long perp, deposit).
    let (threshold_price, cmp) = if slope > 0 {
        (inputs.price as i128 - haircut, 1u8)
    } else {
        (inputs.price as i128 + haircut, 0u8)
    };
    if threshold_price <= 0 {
        return Ok(None);
    }
    let Some(raw) = raw_price(threshold_price, exponent) else {
        return Ok(None);
    };

    let (perp_market_pda, _) = Pubkey::find_program_address(
        &[b"perp_market", target_market_index.to_le_bytes().as_ref()],
        &crate::ID,
    );
    let resolver_accounts = [
        AccountRefV0::writable(conditions_key.to_bytes()),
        AccountRefV0::readonly(user_key.to_bytes()),
        AccountRefV0::readonly(oracle.to_bytes()),
        AccountRefV0::readonly(perp_market_pda.to_bytes()),
    ];
    Ok(Some(ConditionV0::on_value_cross(
        oracle.to_bytes(),
        PYTH_LAZER_PRICE_OFFSET,
        PYTH_LAZER_PRICE_LEN,
        raw,
        cmp,
        CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc,
            executor_program: crate::ID.to_bytes(),
            executor_disc,
            min_payment,
        },
        &resolver_accounts,
    )))
}

/// PRICE_PRECISION → the oracle's raw units.
fn raw_price(price: i128, exponent: i32) -> Option<i64> {
    let raw = match exponent {
        e if !(0..=12).contains(&e) => return None,
        e if e >= 6 => price.checked_mul(10i128.checked_pow((e - 6) as u32)?)?,
        e => price / 10i128.checked_pow((6 - e) as u32)?,
    };
    if raw > i64::MAX as i128 || raw <= 0 {
        return None;
    }
    Some(raw as i64)
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
