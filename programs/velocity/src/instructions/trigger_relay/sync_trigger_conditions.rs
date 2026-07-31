//! Rewrite a user's relay trigger-condition block from their live orders.
//!
//! Permissionless and idempotent — the block is a *hint set*, and this is
//! its only writer besides the trigger cranks' slot release: anyone (the
//! user's UI after placing, a keeper, the user) can call it, paying rent on
//! first touch. Each armed trigger order (up to the slot cap) gets one
//! `OnValueCross` condition watching the market oracle's raw price at the
//! trigger threshold, so relay turners pay nothing while the price is away
//! from the trigger. Orders past the cap, on markets without crank
//! conditions (no reservoir → no keeper fee to express), or on oracle
//! sources without a raw-price watch layout simply stay on the keeper-bot
//! path — the correctness floor either way.
//!
//! `remaining_accounts` carry, in any order: the perp markets of the user's
//! trigger orders, their oracle accounts, their `ClobCrankConditionsV0`
//! (keeper payment), the markets' CLOB entries (for the trigger-limit →
//! CLOB executor), and the user's full margin-map section (every spot/perp
//! market + oracle their positions touch — what the SDK's
//! `getRemainingAccounts` already computes). The margin maps are captured
//! onto the account for the staged executors; they go stale when positions
//! change and a re-sync repairs them.

use {
    crate::{
        error::ErrorCode,
        state::{
            clob_crank::ClobCrankConditionsV0,
            perp_market::PerpMarket,
            prop_amm::{QuoterType, QuoterV0},
            pyth_lazer_oracle::PythLazerOracle,
            spot_market::SpotMarket,
            trigger_conditions::{
                TriggerConditionsV0, TriggerSlotMetaV0, TRIGGER_CONDITIONS_PDA_SEED,
                TRIGGER_CONDITION_SLOTS,
            },
            user::{OrderStatus, OrderType, User},
        },
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::{collections::BTreeMap, convert::TryInto},
};

/// PythLazer raw-price watch layout: `price: i64` at account offset 8 (past
/// the discriminator), `exponent: i32` at offset 32. Other sources have no
/// registered layout yet and fall back to the keeper path.
const PYTH_LAZER_PRICE_OFFSET: u32 = 8;
const PYTH_LAZER_PRICE_LEN: u32 = 8;
const PYTH_LAZER_EXPONENT_OFFSET: usize = 32;

#[derive(Accounts)]
pub struct SyncTriggerConditions<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub user: AccountLoader<'info, User>,
    #[account(
        init_if_needed,
        seeds = [TRIGGER_CONDITIONS_PDA_SEED, user.key().as_ref()],
        space = TriggerConditionsV0::SIZE,
        bump,
        payer = payer
    )]
    pub trigger_conditions: AccountLoader<'info, TriggerConditionsV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

/// The per-market inputs the sync collects from `remaining_accounts`.
#[derive(Default)]
struct MarketInputs {
    oracle: Option<Pubkey>,
    oracle_source_is_lazer: bool,
    /// (raw-price watch offset/len, exponent) read off the oracle account.
    lazer_exponent: Option<i32>,
    keeper_payment_lamports: Option<u64>,
    /// (entry, book, program) when a vetted CLOB is attached.
    clob: Option<(Pubkey, Pubkey, Pubkey)>,
}

pub fn handle_sync_trigger_conditions<'c: 'info, 'info>(
    ctx: Context<'info, SyncTriggerConditions<'info>>,
) -> Result<()> {
    // Classify the remaining accounts by discriminator; oracles are matched
    // by pubkey against the loaded markets afterwards.
    let mut markets: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut market_oracles: BTreeMap<Pubkey, u16> = BTreeMap::new();
    let mut market_refs: Vec<AccountRefV0> = Vec::new();
    let mut oracle_infos: BTreeMap<Pubkey, &AccountInfo<'info>> = BTreeMap::new();

    for info in ctx.remaining_accounts {
        if info.owner == &crate::ID {
            if let Ok(loader) = AccountLoader::<PerpMarket>::try_from(info) {
                let market = loader.load()?;
                let inputs = markets.entry(market.market_index).or_default();
                inputs.oracle = Some(market.oracle);
                inputs.oracle_source_is_lazer = matches!(
                    market.oracle_source,
                    crate::state::oracle::OracleSource::PythLazer
                );
                market_oracles.insert(market.oracle, market.market_index);
                market_refs.push(AccountRefV0::writable(info.key.to_bytes()));
                continue;
            }
            if let Ok(loader) = AccountLoader::<SpotMarket>::try_from(info) {
                let _ = loader.load()?;
                market_refs.push(AccountRefV0::writable(info.key.to_bytes()));
                continue;
            }
            if let Ok(loader) = AccountLoader::<ClobCrankConditionsV0>::try_from(info) {
                let conditions = loader.load()?;
                markets
                    .entry(conditions.market_index)
                    .or_default()
                    .keeper_payment_lamports = Some(conditions.keeper_payment_lamports);
                continue;
            }
            if let Ok(loader) = AccountLoader::<QuoterV0>::try_from(info) {
                let entry = loader.load()?;
                if entry.quoter_type == QuoterType::Clob && entry.is_active && entry.is_approved {
                    markets.entry(entry.market).or_default().clob =
                        Some((*info.key, entry.response_account, entry.program_id));
                }
                continue;
            }
        }
        // Anything else is a candidate oracle: matched by pubkey below, and
        // part of the margin-map section (readonly).
        oracle_infos.insert(*info.key, info);
    }
    // Map section in load_maps order: oracles (readonly) first, then the
    // spot/perp markets (writable). Lazer exponents read off the oracle
    // bytes for the threshold conversion.
    let mut map_refs: Vec<AccountRefV0> = Vec::new();
    for (key, info) in &oracle_infos {
        map_refs.push(AccountRefV0::readonly(key.to_bytes()));
        if let Some(market_index) = market_oracles.get(key) {
            let data = info.try_borrow_data()?;
            if data.len() >= 8 + core::mem::size_of::<PythLazerOracle>()
                && &data[..8] == PythLazerOracle::DISCRIMINATOR
            {
                let exponent = i32::from_le_bytes(
                    data[PYTH_LAZER_EXPONENT_OFFSET..PYTH_LAZER_EXPONENT_OFFSET + 4]
                        .try_into()
                        .map_err(|_| ErrorCode::DefaultError)?,
                );
                markets.entry(*market_index).or_default().lazer_exponent = Some(exponent);
            }
        }
    }
    map_refs.extend(market_refs);

    let user_key = ctx.accounts.user.key();
    let conditions_key = ctx.accounts.trigger_conditions.key();
    let disc8 = |disc: &[u8]| -> Result<[u8; 8]> {
        disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
    };

    let mut conditions = ctx
        .accounts
        .trigger_conditions
        .load_init()
        .or_else(|_| ctx.accounts.trigger_conditions.load_mut())?;
    conditions.user = user_key;
    conditions.init_header()?;
    conditions.write_map_accounts(&map_refs)?;

    let user = crate::load!(ctx.accounts.user)?;
    let mut slot_index = 0usize;
    for order in user.orders.iter() {
        if slot_index >= TRIGGER_CONDITION_SLOTS {
            break;
        }
        if order.status != OrderStatus::Open || !order.must_be_triggered() || order.triggered() {
            continue;
        }
        let Some(inputs) = markets.get(&order.market_index) else {
            continue;
        };
        // Everything a watch needs, or the order stays keeper-only.
        let (Some(oracle), Some(exponent), Some(min_payment)) = (
            inputs.oracle,
            inputs.lazer_exponent,
            inputs.keeper_payment_lamports,
        ) else {
            continue;
        };
        if !inputs.oracle_source_is_lazer {
            continue;
        }
        let Some(threshold) = raw_threshold(order.trigger_price, exponent, order.trigger_condition)
        else {
            continue;
        };
        let cmp = match order.trigger_condition {
            crate::state::user::OrderTriggerCondition::Above => 0u8,
            crate::state::user::OrderTriggerCondition::Below => 1u8,
            _ => continue,
        };

        // Trigger-limits go to the CLOB when the market has one (and the
        // order has a fixed resting price); everything else through the
        // plain trigger crank.
        let clob_path = order.order_type == OrderType::TriggerLimit
            && order.oracle_price_offset == 0
            && inputs.clob.is_some();
        let (resolver_disc, executor_disc, meta) = if clob_path {
            let (entry, book, program) = inputs.clob.unwrap();
            (
                disc8(crate::instruction::ResolveTriggerClobOrder::DISCRIMINATOR)?,
                disc8(crate::instruction::TriggerClobOrder::DISCRIMINATOR)?,
                TriggerSlotMetaV0 {
                    quoter: entry,
                    clob_market: book,
                    clob_program: program,
                    order_id: order.order_id,
                    market_index: order.market_index,
                    padding: [0; 2],
                },
            )
        } else {
            (
                disc8(crate::instruction::ResolveTriggerOrder::DISCRIMINATOR)?,
                disc8(crate::instruction::TriggerOrder::DISCRIMINATOR)?,
                TriggerSlotMetaV0 {
                    quoter: Pubkey::default(),
                    clob_market: Pubkey::default(),
                    clob_program: Pubkey::default(),
                    order_id: order.order_id,
                    market_index: order.market_index,
                    padding: [0; 2],
                },
            )
        };

        let (perp_market_pda, _) = Pubkey::find_program_address(
            &[b"perp_market", order.market_index.to_le_bytes().as_ref()],
            &crate::ID,
        );
        let resolver_accounts = [
            AccountRefV0::writable(conditions_key.to_bytes()),
            AccountRefV0::readonly(user_key.to_bytes()),
            AccountRefV0::readonly(oracle.to_bytes()),
            AccountRefV0::readonly(perp_market_pda.to_bytes()),
        ];
        let spec = CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc,
            executor_program: crate::ID.to_bytes(),
            executor_disc,
            min_payment,
        };
        conditions.write_condition(
            slot_index,
            &ConditionV0::on_value_cross(
                oracle.to_bytes(),
                PYTH_LAZER_PRICE_OFFSET,
                PYTH_LAZER_PRICE_LEN,
                threshold,
                cmp,
                spec,
                &resolver_accounts,
            ),
        )?;
        conditions.slots[slot_index] = meta;
        slot_index += 1;
    }
    // Stale tail slots go quiet.
    for index in slot_index..TRIGGER_CONDITION_SLOTS {
        conditions.write_condition(index, &relay_spec::bytemuck::Zeroable::zeroed())?;
        conditions.slots[index] = TriggerSlotMetaV0::default();
    }
    Ok(())
}

/// A trigger price (PRICE_PRECISION, 1e6) in the oracle's raw units,
/// rounded toward early-firing: `Above` fires when the oracle climbs to
/// the trigger, so its threshold rounds down; `Below` rounds up. Early is
/// a wasted resolver simulation (it re-verifies with the real oracle
/// code); late would be a missed trigger.
fn raw_threshold(
    trigger_price: u64,
    exponent: i32,
    condition: crate::state::user::OrderTriggerCondition,
) -> Option<i64> {
    let price = i128::from(trigger_price);
    let raw = match exponent {
        e if !(0..=12).contains(&e) => return None,
        e if e >= 6 => price.checked_mul(10i128.checked_pow((e - 6) as u32)?)?,
        e => {
            let divisor = 10i128.checked_pow((6 - e) as u32)?;
            match condition {
                crate::state::user::OrderTriggerCondition::Above => price / divisor,
                // Ceiling division (int_roundings is unstable on this
                // toolchain); price and divisor are nonnegative.
                _ => (price + divisor - 1) / divisor,
            }
        }
    };
    if raw > i64::MAX as i128 || raw < i64::MIN as i128 {
        return None;
    }
    Some(raw as i64)
}
