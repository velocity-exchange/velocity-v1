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
//! Each fired trigger routes to one of three executors by order type and
//! whether the market has a CLOB: a trigger-limit rests whole on the book
//! (`trigger_limit_order_v1`), a stop-market fires and fills against the book
//! (`trigger_market_order_v1`), and anything on a market without a CLOB stays on the
//! plain trigger crank (`trigger_order`) for a keeper bot.
//!
//! `remaining_accounts` carry, in any order: the perp markets of the user's
//! trigger orders, their oracle accounts, their `ClobCrankConditionsV0`
//! (keeper payment), the markets' CLOB entries (for the two CLOB executors),
//! and the user's full margin-map section (every spot/perp
//! market + oracle their positions touch — what the SDK's
//! `getRemainingAccounts` already computes). The margin maps are captured
//! onto the account for the staged executors; they go stale when positions
//! change and a re-sync repairs them.
//!
//! The margin-map section is required, not optional. The captured list is the
//! one list every liquidation condition on the account also reads, and this
//! instruction is permissionless. A caller that passed less than the user is
//! exposed in would replace that account's liquidation coverage with a list
//! that fails to load maps. So this pass refuses the same short call the
//! liquidation pass refuses, and it stores the list in the same shape.

use {
    crate::{
        error::ErrorCode,
        instructions::{validate_market_coverage, MarketCoverage},
        state::{
            clob_crank::ClobCrankConditionsV0,
            oracle::OracleSource,
            oracle_watch::{oracle_watch, OracleWatchV0, WatchDirection},
            perp_market::PerpMarket,
            prop_amm::{clob_slot_index, QuoterSlabExt, QuoterSlabV0},
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
    std::collections::BTreeMap,
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
    /// True when the market has a CLOB attached, which is also when it has a
    /// crank conditions account.
    has_clob: bool,
    /// Where a raw-price watch reads this market's oracle, when its source
    /// has a registered layout. `None` leaves the order keeper-only.
    watch: Option<OracleWatchV0>,
    keeper_payment_lamports: Option<u64>,
    /// (quoter slab, book, program) when a vetted CLOB is attached.
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
///
/// The list is the whole of the account's liquidation coverage: every
/// liquidation condition and every staged executor reads it, and a write
/// replaces it. This pass is permissionless, so writing it has the same
/// precondition the liquidation pass carries. A caller that passes fewer
/// accounts than the user is exposed in is refused rather than allowed to
/// leave a list that fails to load maps.
pub fn rewrite_trigger_conditions<'info>(
    trigger_conditions: &AccountLoader<'info, UserConditionsV0>,
    user_loader: &AccountLoader<'info, User>,
    remaining_accounts: &'info [AccountInfo<'info>],
    write_shared_list: bool,
) -> Result<()> {
    let mut inputs = collect_trigger_inputs(remaining_accounts)?;
    if write_shared_list {
        validate_market_coverage(user_loader, &inputs.coverage())?;
    }
    let oracle_refs = resolve_oracle_watches(&mut inputs);

    let user_key = user_loader.key();
    let conditions_key = trigger_conditions.key();

    let mut conditions = trigger_conditions
        .load_init()
        .or_else(|_| trigger_conditions.load_mut())?;
    conditions.user = user_key;
    conditions.init_block()?;
    if write_shared_list {
        let stored = build_sync_accounts(
            conditions_key,
            user_key,
            oracle_refs,
            inputs.market_refs,
            inputs.tail_refs,
        );
        conditions.write_sync_accounts(&stored)?;
    }

    let user = crate::load!(user_loader)?;
    let mut slot_index = 0usize;
    for order in user.orders.iter() {
        if slot_index >= TRIGGER_CONDITION_SLOTS {
            break;
        }
        let Some(trigger) = trigger_watch_for_order(order, &inputs.markets) else {
            continue;
        };
        let (resolver_disc, meta) = route_trigger_resolver(order, trigger.clob)?;
        let resolvers = conditions.write_slot_resolvers(
            slot_index,
            &slot_resolver_refs(conditions_key, user_key, trigger.oracle, order.market_index),
        )?;
        let spec = CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc,
            min_payment: trigger.min_payment,
        };
        conditions.set_condition(
            TRIGGER_SLOT_BASE + slot_index,
            &ConditionV0::on_value_cross(
                relay_spec::WatchedRegion::new(
                    trigger.oracle.to_bytes(),
                    trigger.watch.price_offset,
                    trigger.watch.price_len,
                ),
                // Signed: every registered watch layout stores its price
                // as i64.
                relay_spec::WatchValue::Signed(trigger.threshold),
                trigger.cmp,
                spec,
                resolvers,
            ),
        )?;
        conditions.trigger_slots[slot_index] = meta;
        slot_index += 1;
    }
    clear_unused_trigger_slots(&mut conditions, slot_index)
}

/// The classified `remaining_accounts`: the per-market inputs a trigger
/// watch is derived from, and the market references the stored list
/// carries.
struct TriggerInputs<'info> {
    markets: BTreeMap<u16, MarketInputs>,
    market_oracles: BTreeMap<Pubkey, u16>,
    /// Oracle each given spot market names. Read by the coverage rule only.
    spot_oracles: BTreeMap<u16, Pubkey>,
    market_refs: Vec<AccountRefV0>,
    /// The crank accounts the stored list carries after the margin map, in the
    /// same order and shape the liquidation pass stores them.
    tail_refs: Vec<AccountRefV0>,
    oracle_infos: BTreeMap<Pubkey, &'info AccountInfo<'info>>,
}

impl TriggerInputs<'_> {
    /// The view the shared coverage rule answers over.
    fn coverage(&self) -> MarketCoverage {
        MarketCoverage {
            perp_oracles: self
                .markets
                .iter()
                .filter_map(|(index, inputs)| Some((*index, inputs.oracle?)))
                .collect(),
            spot_oracles: self.spot_oracles.clone(),
            perp_books: self
                .markets
                .iter()
                .filter(|(_, inputs)| inputs.has_clob)
                .map(|(index, _)| *index)
                .collect(),
            perp_cranks: self
                .markets
                .iter()
                .filter(|(_, inputs)| inputs.keeper_payment_lamports.is_some())
                .map(|(index, _)| *index)
                .collect(),
            oracles: self.oracle_infos.keys().copied().collect(),
        }
    }
}

/// Classify the remaining accounts by discriminator; oracles are matched
/// by pubkey against the loaded markets afterwards.
fn collect_trigger_inputs<'info>(
    remaining_accounts: &'info [AccountInfo<'info>],
) -> Result<TriggerInputs<'info>> {
    let mut markets: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut market_oracles: BTreeMap<Pubkey, u16> = BTreeMap::new();
    let mut spot_oracles: BTreeMap<u16, Pubkey> = BTreeMap::new();
    let mut market_refs: Vec<AccountRefV0> = Vec::new();
    let mut tail_refs: Vec<AccountRefV0> = Vec::new();
    let mut oracle_infos: BTreeMap<Pubkey, &AccountInfo<'info>> = BTreeMap::new();

    for info in remaining_accounts {
        if info.owner == &crate::ID {
            if let Ok(loader) = AccountLoader::<PerpMarket>::try_from(info) {
                let market = loader.load()?;
                let inputs = markets.entry(market.market_index).or_default();
                inputs.oracle = Some(market.oracle);
                inputs.oracle_source = Some(market.oracle_source);
                inputs.has_clob = market.clob_market != Pubkey::default();
                market_oracles.insert(market.oracle, market.market_index);
                market_refs.push(AccountRefV0::writable(info.key.to_bytes()));
                continue;
            }
            if let Ok(loader) = AccountLoader::<SpotMarket>::try_from(info) {
                let market = loader.load()?;
                spot_oracles.insert(market.market_index, market.oracle);
                market_refs.push(AccountRefV0::writable(info.key.to_bytes()));
                continue;
            }
            if let Ok(loader) = AccountLoader::<ClobCrankConditionsV0>::try_from(info) {
                let conditions = loader.load()?;
                markets
                    .entry(conditions.market_index)
                    .or_default()
                    .keeper_payment_lamports = Some(u64::from(conditions.crank_payments.trigger));
                tail_refs.push(AccountRefV0::readonly(info.key.to_bytes()));
                continue;
            }
            if let Ok(loader) = AccountLoader::<QuoterSlabV0>::try_from(info) {
                let market = loader.load()?.market;
                let slots = loader.slots()?;
                // Stored after the markets, where the map parser never
                // reaches. The liquidation pass stores the slab, its book and
                // the book's program in this order, and both passes write the
                // same list, so this pass carries them the same way.
                tail_refs.push(AccountRefV0::readonly(info.key.to_bytes()));
                if let Some(index) = clob_slot_index(&slots) {
                    tail_refs.push(AccountRefV0::writable(
                        slots[index].config.response_account.to_bytes(),
                    ));
                    tail_refs.push(AccountRefV0::readonly(
                        slots[index].config.program_id.to_bytes(),
                    ));
                    if slots[index].quotes() {
                        markets.entry(market).or_default().clob = Some((
                            *info.key,
                            slots[index].config.response_account,
                            slots[index].config.program_id,
                        ));
                    }
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

    Ok(TriggerInputs {
        markets,
        market_oracles,
        spot_oracles,
        market_refs,
        tail_refs,
        oracle_infos,
    })
}

/// The watch layout is resolved off each oracle's bytes here, once, for
/// the threshold conversion each armed order needs. The refs come back in
/// map order, oracles first and readonly.
fn resolve_oracle_watches(inputs: &mut TriggerInputs<'_>) -> Vec<AccountRefV0> {
    let mut oracle_refs: Vec<AccountRefV0> = Vec::new();
    for (key, info) in &inputs.oracle_infos {
        oracle_refs.push(AccountRefV0::readonly(key.to_bytes()));
        let Some(market_index) = inputs.market_oracles.get(key) else {
            continue;
        };
        let entry = inputs.markets.entry(*market_index).or_default();
        if let Some(source) = entry.oracle_source {
            entry.watch = oracle_watch(info, source);
        }
    }
    oracle_refs
}

/// The same stored list the liquidation sync writes: the resolver's
/// named accounts, then the user's margin map. Either sync populating
/// it is enough, and neither has to carry its own copy.
///
/// The map section follows load_maps order: oracles (readonly) first, then
/// the spot/perp markets (writable), then the crank tail the map parser never
/// reaches. Both passes build the list the same way from the same accounts,
/// so neither can write a weaker one than the other.
fn build_sync_accounts(
    conditions_key: Pubkey,
    user_key: Pubkey,
    oracle_refs: Vec<AccountRefV0>,
    market_refs: Vec<AccountRefV0>,
    tail_refs: Vec<AccountRefV0>,
) -> Vec<AccountRefV0> {
    let mut stored = vec![
        AccountRefV0::writable(crate::state::pdas::relay_scratch().to_bytes()),
        AccountRefV0::readonly(conditions_key.to_bytes()),
        AccountRefV0::readonly(user_key.to_bytes()),
        AccountRefV0::readonly(crate::state::pdas::state().to_bytes()),
    ];
    stored.extend(oracle_refs);
    stored.extend(market_refs);
    stored.extend(tail_refs);
    stored
}

/// The raw-price watch one armed trigger order rides on.
struct TriggerWatch {
    oracle: Pubkey,
    watch: OracleWatchV0,
    /// What the market's crank conditions pay for the trigger.
    min_payment: u64,
    /// The trigger price in the oracle's raw units.
    threshold: i64,
    /// The comparison byte `ConditionV0::on_value_cross` fires on.
    cmp: u8,
    /// (quoter slab, book, program) when a vetted CLOB is attached.
    clob: Option<(Pubkey, Pubkey, Pubkey)>,
}

/// Derive the watch that arms one order. `None` leaves the order on the
/// keeper-bot path: it does not trigger, its market is absent, or the
/// market gives no oracle, no watch layout, or no keeper payment.
fn trigger_watch_for_order(
    order: &crate::state::user::Order,
    markets: &BTreeMap<u16, MarketInputs>,
) -> Option<TriggerWatch> {
    // Skip a trigger already resting on a book: it deliberately reads
    // as untriggered, so without this the watch re-fires every round
    // and `trigger_limit_order_v1` rejects the staged crank each time.
    if order.status != OrderStatus::Open
        || !order.must_be_triggered()
        || order.triggered()
        || order.is_placed_on_clob()
    {
        return None;
    }
    let inputs = markets.get(&order.market_index)?;
    // Everything a watch needs, or the order stays keeper-only.
    let (Some(oracle), Some(watch), Some(min_payment)) =
        (inputs.oracle, inputs.watch, inputs.keeper_payment_lamports)
    else {
        return None;
    };
    let direction = match order.trigger_condition {
        crate::state::user::OrderTriggerCondition::Above => WatchDirection::AtOrAbove,
        crate::state::user::OrderTriggerCondition::Below => WatchDirection::AtOrBelow,
        _ => return None,
    };
    let threshold = watch.raw_threshold(i128::from(order.trigger_price), direction)?;
    Some(TriggerWatch {
        oracle,
        watch,
        min_payment,
        threshold,
        cmp: direction.cmp(),
        clob: inputs.clob,
    })
}

/// Route each fired trigger to its resolver by order type and whether
/// the market has a CLOB. A trigger-limit with a fixed resting price
/// rests whole on the book (`trigger_limit_order_v1`). A stop-market fires
/// and fills against the book (`trigger_market_order_v1`). Everything else —
/// any trigger on a market without a CLOB, or a trigger-limit with an
/// oracle offset that cannot rest at a fixed price — stays on the plain
/// trigger crank for a keeper bot to fill.
fn route_trigger_resolver(
    order: &crate::state::user::Order,
    clob: Option<(Pubkey, Pubkey, Pubkey)>,
) -> Result<([u8; 8], TriggerSlotMetaV0)> {
    let clob_rest = order.order_type == OrderType::TriggerLimit
        && order.oracle_price_offset == 0
        && clob.is_some();
    let clob_fill = order.order_type == OrderType::TriggerMarket && clob.is_some();
    if clob_rest || clob_fill {
        let (slab, book, program) = clob.unwrap();
        let disc = if clob_rest {
            crate::instruction::ResolveTriggerLimitOrderV1::DISCRIMINATOR
        } else {
            crate::instruction::ResolveTriggerMarketOrderV1::DISCRIMINATOR
        };
        Ok((
            crate::instructions::relay_harness::disc8(disc)?,
            TriggerSlotMetaV0 {
                quoter_slab: slab,
                clob_market: book,
                clob_program: program,
                order_id: order.order_id,
                market_index: order.market_index,
                padding: [0; 2],
            },
        ))
    } else {
        Ok((
            crate::instructions::relay_harness::disc8(
                crate::instruction::ResolveTriggerOrder::DISCRIMINATOR,
            )?,
            TriggerSlotMetaV0 {
                quoter_slab: Pubkey::default(),
                clob_market: Pubkey::default(),
                clob_program: Pubkey::default(),
                order_id: order.order_id,
                market_index: order.market_index,
                padding: [0; 2],
            },
        ))
    }
}

/// Every trigger resolver shares one account set: the scratch, the
/// block, the user, and the slot's own oracle and perp market. The
/// fire-to-book resolver stages a taker-origin rest, not a fill, so it
/// reads no book and needs no CLOB accounts of its own.
fn slot_resolver_refs(
    conditions_key: Pubkey,
    user_key: Pubkey,
    oracle: Pubkey,
    market_index: u16,
) -> [AccountRefV0; 5] {
    let (perp_market_pda, _) = Pubkey::find_program_address(
        &[b"perp_market", market_index.to_le_bytes().as_ref()],
        &crate::ID,
    );
    [
        AccountRefV0::writable(crate::state::pdas::relay_scratch().to_bytes()),
        AccountRefV0::readonly(conditions_key.to_bytes()),
        AccountRefV0::readonly(user_key.to_bytes()),
        AccountRefV0::readonly(oracle.to_bytes()),
        AccountRefV0::readonly(perp_market_pda.to_bytes()),
    ]
}

/// Stale tail slots go quiet.
fn clear_unused_trigger_slots(conditions: &mut UserConditionsV0, from: usize) -> Result<()> {
    for index in from..TRIGGER_CONDITION_SLOTS {
        conditions.set_condition(
            TRIGGER_SLOT_BASE + index,
            &relay_spec::bytemuck::Zeroable::zeroed(),
        )?;
        conditions.trigger_slots[index] = TriggerSlotMetaV0::default();
    }
    Ok(())
}
