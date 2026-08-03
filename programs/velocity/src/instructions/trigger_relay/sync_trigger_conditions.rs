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
            oracle::OracleSource,
            oracle_watch::{oracle_watch, OracleWatchV0, WatchDirection},
            perp_market::PerpMarket,
            prop_amm::{QuoterType, QuoterV0},
            spot_market::SpotMarket,
            user::{OrderStatus, OrderType, User},
            user_conditions::{
                TriggerSlotMetaV0, UserConditionsV0, TRIGGER_CONDITION_SLOTS, TRIGGER_SLOT_BASE,
                USER_CONDITIONS_PDA_SEED,
            },
        },
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::{collections::BTreeMap, convert::TryInto},
};

#[derive(Accounts)]
pub struct SyncTriggerConditions<'info> {
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
    pub trigger_conditions: AccountLoader<'info, UserConditionsV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

/// The per-market inputs the sync collects from `remaining_accounts`.
#[derive(Default)]
struct MarketInputs {
    oracle: Option<Pubkey>,
    oracle_source: Option<OracleSource>,
    /// Where a raw-price watch reads this market's oracle, when its source
    /// has a registered layout. `None` leaves the order keeper-only.
    watch: Option<OracleWatchV0>,
    keeper_payment_lamports: Option<u64>,
    /// (entry, book, program) when a vetted CLOB is attached.
    clob: Option<(Pubkey, Pubkey, Pubkey)>,
}

pub fn handle_sync_trigger_conditions<'c: 'info, 'info>(
    ctx: Context<'info, SyncTriggerConditions<'info>>,
) -> Result<()> {
    rewrite_trigger_conditions(
        &ctx.accounts.trigger_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        true,
    )
}

/// Derive this user's trigger conditions into the trigger slot range.
///
/// `write_shared_list` is false when the liquidation pass in the same
/// instruction already wrote the resolver list — it is the same list, and
/// writing it twice is just CU.
pub fn rewrite_trigger_conditions<'info>(
    trigger_conditions: &AccountLoader<'info, UserConditionsV0>,
    user_loader: &AccountLoader<'info, User>,
    remaining_accounts: &'info [AccountInfo<'info>],
    write_shared_list: bool,
) -> Result<()> {
    // Classify the remaining accounts by discriminator; oracles are matched
    // by pubkey against the loaded markets afterwards.
    let mut markets: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut market_oracles: BTreeMap<Pubkey, u16> = BTreeMap::new();
    let mut market_refs: Vec<AccountRefV0> = Vec::new();
    let mut oracle_infos: BTreeMap<Pubkey, &AccountInfo<'info>> = BTreeMap::new();

    for info in remaining_accounts {
        if info.owner == &crate::ID {
            if let Ok(loader) = AccountLoader::<PerpMarket>::try_from(info) {
                let market = loader.load()?;
                let inputs = markets.entry(market.market_index).or_default();
                inputs.oracle = Some(market.oracle);
                inputs.oracle_source = Some(market.oracle_source);
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
        // part of the margin-map section (readonly). Velocity-owned
        // accounts land here too — deliberately: velocity hosts its own
        // oracle accounts (PythLazer, prelaunch).
        oracle_infos.insert(*info.key, info);
    }
    // Map section in load_maps order: oracles (readonly) first, then the
    // spot/perp markets (writable). The watch layout is resolved off each
    // oracle's bytes here, once, for the threshold conversion below.
    let mut map_refs: Vec<AccountRefV0> = Vec::new();
    for (key, info) in &oracle_infos {
        map_refs.push(AccountRefV0::readonly(key.to_bytes()));
        let Some(market_index) = market_oracles.get(key) else {
            continue;
        };
        let entry = markets.entry(*market_index).or_default();
        if let Some(source) = entry.oracle_source {
            entry.watch = oracle_watch(info, source);
        }
    }
    map_refs.extend(market_refs);

    let user_key = user_loader.key();
    let conditions_key = trigger_conditions.key();
    let disc8 = |disc: &[u8]| -> Result<[u8; 8]> {
        disc.try_into().map_err(|_| error!(ErrorCode::DefaultError))
    };

    let mut conditions = trigger_conditions
        .load_init()
        .or_else(|_| trigger_conditions.load_mut())?;
    conditions.user = user_key;
    conditions.init_block()?;
    // The same stored list the liquidation sync writes: the resolver's
    // named accounts, then the user's margin map. Either sync populating
    // it is enough, and neither has to carry its own copy.
    let mut stored = vec![
        AccountRefV0::writable(crate::state::pdas::relay_scratch().to_bytes()),
        AccountRefV0::readonly(conditions_key.to_bytes()),
        AccountRefV0::readonly(user_key.to_bytes()),
        AccountRefV0::readonly(crate::state::pdas::state().to_bytes()),
    ];
    stored.extend(map_refs);
    if write_shared_list {
        conditions.write_sync_accounts(&stored)?;
    }

    let user = crate::load!(user_loader)?;
    let mut slot_index = 0usize;
    for order in user.orders.iter() {
        if slot_index >= TRIGGER_CONDITION_SLOTS {
            break;
        }
        // Skip a trigger already resting on a book: it deliberately reads
        // as untriggered, so without this the watch re-fires every round
        // and `trigger_clob_order` rejects the staged crank each time.
        if order.status != OrderStatus::Open
            || !order.must_be_triggered()
            || order.triggered()
            || order.is_placed_on_clob()
        {
            continue;
        }
        let Some(inputs) = markets.get(&order.market_index) else {
            continue;
        };
        // Everything a watch needs, or the order stays keeper-only.
        let (Some(oracle), Some(watch), Some(min_payment)) =
            (inputs.oracle, inputs.watch, inputs.keeper_payment_lamports)
        else {
            continue;
        };
        let direction = match order.trigger_condition {
            crate::state::user::OrderTriggerCondition::Above => WatchDirection::AtOrAbove,
            crate::state::user::OrderTriggerCondition::Below => WatchDirection::AtOrBelow,
            _ => continue,
        };
        let Some(threshold) = watch.raw_threshold(i128::from(order.trigger_price), direction)
        else {
            continue;
        };
        let cmp = direction.cmp();

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
        let resolvers = conditions.write_slot_resolvers(
            slot_index,
            &[
                AccountRefV0::writable(crate::state::pdas::relay_scratch().to_bytes()),
                AccountRefV0::readonly(conditions_key.to_bytes()),
                AccountRefV0::readonly(user_key.to_bytes()),
                AccountRefV0::readonly(oracle.to_bytes()),
                AccountRefV0::readonly(perp_market_pda.to_bytes()),
            ],
        )?;
        let spec = CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc,
            executor_program: crate::ID.to_bytes(),
            executor_disc,
            min_payment,
        };
        conditions.set_condition(
            TRIGGER_SLOT_BASE + slot_index,
            &ConditionV0::on_value_cross(
                oracle.to_bytes(),
                watch.price_offset,
                watch.price_len,
                // Signed: every registered watch layout stores its price
                // as i64.
                relay_spec::WatchValue::Signed(threshold),
                cmp,
                spec,
                resolvers,
            ),
        )?;
        conditions.trigger_slots[slot_index] = meta;
        slot_index += 1;
    }
    // Stale tail slots go quiet.
    for index in slot_index..TRIGGER_CONDITION_SLOTS {
        conditions.set_condition(
            TRIGGER_SLOT_BASE + index,
            &relay_spec::bytemuck::Zeroable::zeroed(),
        )?;
        conditions.trigger_slots[index] = TriggerSlotMetaV0::default();
    }
    Ok(())
}
