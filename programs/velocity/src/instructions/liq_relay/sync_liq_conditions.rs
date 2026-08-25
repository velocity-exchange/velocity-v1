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
        math::constants::PRICE_PRECISION_I128,
        state::{
            clob_crank::ClobCrankConditionsV0,
            oracle::OracleSource,
            oracle_watch::{oracle_watch, OracleWatchV0},
            pdas,
            perp_market::PerpMarket,
            prop_amm::QuoterV0,
            spot_market::SpotMarket,
            state::State,
            user::User,
            user_conditions::{
                UserConditionsV0, LIQ_LIVENESS_POLL, LIQ_LIVENESS_POLL_SLOTS, LIQ_SYNC_FALLBACK,
                LIQ_SYNC_WATCH, USER_CONDITIONS_PDA_SEED,
            },
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::{collections::BTreeMap, convert::TryInto},
};

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
    /// Read for the fee rails the sync's own keeper payment is priced from.
    pub state: AccountLoader<'info, State>,
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

/// What the caller asks for; the terms the account ends up holding are
/// [`SyncLiqConditionsTerms`], derived from these.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct SyncLiqConditionsArgs {
    /// Cost units the staged self-sync requests, measured by simulating it.
    /// Priced against `State.transaction_fee_rails`. 0 keeps the watch/poll
    /// conditions inactive (manual syncs only) — turners have no signal to
    /// take unpaid work.
    pub sync_cost_units: u32,
    /// Coarse fallback interval, in slots. 0 = use the previous value.
    pub sync_fallback_slots: u64,
}

/// The terms a block is written with: a lamport fee and a poll interval.
///
/// Separate from [`SyncLiqConditionsArgs`] because the two callers reach them
/// differently. The opt-in sync prices its cost units against the live rails.
/// The staged resync re-reads what the account already holds — re-deriving
/// would let a staged executor re-price its own work.
#[derive(Clone, Copy)]
pub struct SyncLiqConditionsTerms {
    pub sync_payment_lamports: u64,
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
    let state_rails = ctx.accounts.state.load()?.transaction_fee_rails;
    let terms = SyncLiqConditionsTerms {
        sync_payment_lamports: state_rails.transaction_cost(u64::from(args.sync_cost_units), 1)?,
        sync_fallback_slots: args.sync_fallback_slots,
    };
    // The opt-in caller pays its own way: it submitted the transaction, so
    // there is no keeper to reward. The fee belongs to the relay path, where
    // somebody else does the work, and the protocol treasury pays it.
    // `stamp_sync_slot`: this sync brings the block up to date, so the next
    // resync the treasury pays for is an interval away. It also stops a fresh
    // account reading as overdue from slot zero. Stamped inside the rewrite
    // because a newly initialized account's discriminator is not visible to a
    // second load in the same instruction.
    rewrite_liq_conditions(
        &ctx.accounts.liq_conditions,
        &ctx.accounts.user,
        ctx.remaining_accounts,
        terms,
        true,
    )
}

/// Recompute the block from the user's live positions. Shared by the
/// opt-in sync and relay's unsigned resync.
pub fn rewrite_liq_conditions<'info>(
    liq_conditions: &AccountLoader<'info, UserConditionsV0>,
    user_loader: &AccountLoader<'info, User>,
    remaining_accounts: &'info [AccountInfo<'info>],
    args: SyncLiqConditionsTerms,
    stamp_sync_slot: bool,
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
                    .keeper_payment_lamports =
                    Some(u64::from(conditions.crank_payments.liquidation));
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
    if stamp_sync_slot {
        conditions.last_paid_sync_slot = Clock::get()?.slot;
    }
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
    // The whole of velocity's liquidation coverage for this account. It
    // predicts nothing: it wakes on a clock and lets the resolver run the real
    // maintenance-margin calculation, the same code the executor runs, which
    // reports no work when the account is healthy.
    //
    // What this account carries for the resolver is the margin map — the
    // markets and oracles that calculation needs — and keeping that list
    // current is what the watch and the fallback above are for.
    //
    // Priced at the cheapest liquidation any of the user's markets pays. Relay
    // holds a keeper's balance growth to the floor a condition advertises, so
    // a floor above what the market actually pays would fail the crank it
    // asked for.
    // Priced at the cheapest liquidation any of this user's markets pays.
    // Relay holds a keeper's balance growth to the floor a condition
    // advertises, so a floor above what the market actually pays would fail
    // the crank it asked for.
    let poll_payment = perps
        .values()
        .filter_map(|inputs| inputs.keeper_payment_lamports)
        .min()
        .unwrap_or(0);
    conditions.set_condition(
        LIQ_LIVENESS_POLL,
        &ConditionV0::every_slots(
            LIQ_LIVENESS_POLL_SLOTS,
            CrankSpecV0 {
                resolver_program: crate::ID.to_bytes(),
                resolver_disc: disc8(
                    crate::instruction::ResolveLiquidatePerpWithFill::DISCRIMINATOR,
                )?,
                min_payment: poll_payment,
            },
            resolvers,
        ),
    )?;

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

// Referenced by the doc comment's precision notes.
const _: () = {
    let _ = PRICE_PRECISION_I128;
};

pub fn validate_sync_args(args: &SyncLiqConditionsArgs) -> Result<()> {
    validate!(
        args.sync_fallback_slots > 0 || args.sync_cost_units == 0,
        ErrorCode::DefaultError,
        "a paid self-sync needs a fallback interval"
    )?;
    // The treasury pays this figure to whoever cranks the resync, and opting
    // in is permissionless. Without a ceiling a caller could name its own
    // price against protocol funds and collect it by cranking itself.
    validate!(
        args.sync_cost_units <= crate::state::user_conditions::LIQ_SYNC_MAX_COST_UNITS,
        ErrorCode::DefaultError,
        "self-sync priced at {} cost units, above the {} ceiling",
        args.sync_cost_units,
        crate::state::user_conditions::LIQ_SYNC_MAX_COST_UNITS
    )?;
    // The interval is also how often the treasury will pay for this account,
    // so a paid sync may not name one short enough to be paid every slot.
    validate!(
        args.sync_cost_units == 0
            || args.sync_fallback_slots
                >= crate::state::user_conditions::LIQ_SYNC_MIN_FALLBACK_SLOTS,
        ErrorCode::DefaultError,
        "a paid self-sync needs an interval of at least {} slots, not {}",
        crate::state::user_conditions::LIQ_SYNC_MIN_FALLBACK_SLOTS,
        args.sync_fallback_slots
    )?;
    Ok(())
}
