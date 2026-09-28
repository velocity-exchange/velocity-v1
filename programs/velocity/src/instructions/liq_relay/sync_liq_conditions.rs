//! Rewrite a user's relay liquidation-condition block from their live
//! positions.
//!
//! Writing the block is permissionless and idempotent. Only the user's
//! authority, its delegate or the admin sets or clears paid self-sync terms,
//! because the protocol treasury pays them. This is the opt-in entry point. It creates the account, so it takes a `payer: Signer`. Relay's
//! self-maintenance path is a separate instruction,
//! [`super::resync_liq_conditions`], which names no signer at all. A staged
//! executor is submitted unsigned, because relay's turner marks every meta
//! non-signing and refuses to sign a transaction whose executor names a
//! signer. Both entry points share [`rewrite_liq_conditions`].
//!
//! ## What the block arms
//!
//! A liveness poll wakes `ResolveLiquidatePerpWithFill` on a fixed slot
//! interval. The poll predicts nothing. The resolver runs the full
//! maintenance-margin calculation and reports no work while the account is
//! healthy.
//!
//! The block also arms its own maintenance. A watch over the user's position
//! bytes re-runs the sync when a trade, a deposit or a settlement changes
//! them. A coarse fallback poll catches whatever the watch misses. Both keep
//! the stored account list current, because that list is the margin map the
//! resolver loads.

use {
    crate::{
        error::ErrorCode,
        state::{
            clob_crank::ClobCrankConditionsV0,
            pdas,
            perp_market::PerpMarket,
            prop_amm::{QuoterSlabExt, QuoterSlabV0},
            spot_market::SpotMarket,
            state::{State, TransactionFeeRails},
            user::User,
            user_conditions::{
                UserConditionsV0, LIQ_LIVENESS_POLL, LIQ_LIVENESS_POLL_SLOTS, LIQ_SYNC_FALLBACK,
                LIQ_SYNC_MIN_FALLBACK_SLOTS, LIQ_SYNC_WATCH, USER_CONDITIONS_PDA_SEED,
            },
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::collections::{BTreeMap, BTreeSet},
};

/// The region of the user account a sync watch covers. It runs from
/// `spot_positions` to the start of `orders`, so it holds both position
/// arrays. A trade, a deposit or a settlement writes here. Most unrelated
/// writes, such as the last active slot, do not.
pub fn user_positions_watch_region() -> (u32, u32) {
    let start = 8 + core::mem::offset_of!(User, spot_positions);
    let end = 8 + core::mem::offset_of!(User, orders);
    (start as u32, (end - start) as u32)
}

#[derive(Accounts)]
pub struct SyncLiqConditions<'info> {
    /// Whoever runs the sync. It pays the rent when this call creates the
    /// conditions account, so it is writable.
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

/// What the caller asks for. [`SyncLiqConditionsTerms`] holds the terms the
/// account ends up with, derived from these.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct SyncLiqConditionsArgs {
    /// Cost units the staged self-sync requests, measured by simulating it.
    /// Priced against `State.transaction_fee_rails`. Zero leaves the watch and
    /// the fallback poll inactive, so only a manual sync updates the block. A
    /// turner has no signal to take unpaid work.
    pub sync_cost_units: u32,
    /// Coarse fallback interval, in slots. Zero keeps the previous value.
    pub sync_fallback_slots: u64,
}

/// The terms a block is written with, a lamport fee and a poll interval.
///
/// Separate from [`SyncLiqConditionsArgs`] because the staged resync re-reads the
/// stored terms. Re-deriving would let a staged executor re-price its own work.
#[derive(Clone, Copy)]
pub struct SyncLiqConditionsTerms {
    pub sync_payment_lamports: u64,
    pub sync_fallback_slots: u64,
}

impl SyncLiqConditionsTerms {
    /// True when a paid block also names an interval at or above the floor.
    ///
    /// The interval rate-limits what the treasury pays for one account. A
    /// payment with a one-slot interval is a paid loop anyone can crank.
    pub fn interval_is_sound(&self) -> bool {
        self.sync_payment_lamports == 0 || self.sync_fallback_slots >= LIQ_SYNC_MIN_FALLBACK_SLOTS
    }

    /// What the treasury may pay for a block holding these terms.
    ///
    /// Terms that break the interval floor pay nothing, so a block armed with
    /// such terms cannot drain the treasury. The next rewrite stores the zero.
    pub fn payable_lamports(&self) -> u64 {
        if self.interval_is_sound() {
            self.sync_payment_lamports
        } else {
            0
        }
    }

    /// Refuse terms a block must never hold.
    pub fn validate(&self) -> Result<()> {
        validate!(
            self.interval_is_sound(),
            ErrorCode::SelfSyncIntervalTooShort,
            "a self-sync paying {} lamports needs an interval of at least {} slots, not {}",
            self.sync_payment_lamports,
            LIQ_SYNC_MIN_FALLBACK_SLOTS,
            self.sync_fallback_slots
        )?;

        Ok(())
    }
}

/// Turn a caller's arguments into the terms a block holds. Both opt-in entry
/// points price a self-sync here.
///
/// Zero cost units means an unpaid opt-in. `transaction_cost(0, 1)` is not
/// zero, because it is the signature fee. Pricing zero units through the rails
/// would arm the fallback poll to pay that signature every interval, for an
/// account that named no work.
pub fn price_sync_terms(
    rails: &TransactionFeeRails,
    args: &SyncLiqConditionsArgs,
) -> Result<SyncLiqConditionsTerms> {
    let terms = SyncLiqConditionsTerms {
        sync_payment_lamports: if args.sync_cost_units == 0 {
            0
        } else {
            rails.transaction_cost(u64::from(args.sync_cost_units), 1)?
        },

        sync_fallback_slots: args.sync_fallback_slots,
    };

    terms.validate()?;
    Ok(terms)
}

/// Who runs an opt-in sync, and the block it writes. The treasury pays a
/// block's terms, and a block without terms has no self-maintenance. So only
/// the user's authority, its delegate or the admin sets or clears paid terms.
pub struct SyncCaller<'a, 'info> {
    pub payer: &'a Signer<'info>,
    pub state: &'a AccountLoader<'info, State>,
    pub user: &'a AccountLoader<'info, User>,
    pub conditions: &'a AccountLoader<'info, UserConditionsV0>,
}

impl SyncCaller<'_, '_> {
    /// Price `args`, and refuse terms this caller may not write.
    pub fn authorized_terms(&self, args: &SyncLiqConditionsArgs) -> Result<SyncLiqConditionsTerms> {
        let terms = price_sync_terms(&self.state.load()?.transaction_fee_rails, args)?;
        let may_set_terms =
            crate::instructions::constraints::can_sign_for_user(self.user, self.payer)?
                || crate::auth::check_hot(
                    self.payer.key,
                    self.state,
                    crate::state::state::HotRole::ConditionsSync,
                )?;
        // A block this instruction creates has no discriminator yet, so its
        // load fails. Such a block holds no terms.
        let stored_payment = self
            .conditions
            .load()
            .map_or(0, |conditions| conditions.sync_payment_lamports);

        validate_terms_change(may_set_terms, stored_payment, &terms)?;
        Ok(terms)
    }
}

/// Refuse a third party that would set paid terms or clear stored ones.
pub fn validate_terms_change(
    may_set_terms: bool,
    stored_payment: u64,
    terms: &SyncLiqConditionsTerms,
) -> Result<()> {
    validate!(
        may_set_terms || (stored_payment == 0 && terms.sync_payment_lamports == 0),
        ErrorCode::SelfSyncTermsNeedUserAuthority,
        "a sync by a third party cannot change the paid terms ({} lamports stored, {} asked)",
        stored_payment,
        terms.sync_payment_lamports
    )?;

    Ok(())
}

/// What one market's reservoir pays the cranks the liveness poll stages.
///
/// The poll stages whichever of the two the resolver finds work for, so the
/// figure it advertises has to be one both cranks meet.
#[derive(Clone, Copy)]
struct CrankPayment {
    liquidation: u64,
    force_cancel: u64,
}

/// One market's inputs, collected from `remaining_accounts`.
#[derive(Default, Clone, Copy)]
struct MarketInputs {
    /// The market account. `None` when only its crank account or slab rode
    /// along.
    market: Option<Pubkey>,
    oracle: Option<Pubkey>,
    quote_spot_market_index: Option<u16>,
    /// True when the market has a CLOB attached. A market with a CLOB has a
    /// crank conditions account, and is the only kind of market the staged
    /// liquidation can fill against.
    has_clob: bool,
    /// What this market's conditions account pays. `None` when that account
    /// did not ride along.
    crank_payment: Option<CrankPayment>,
    /// True when this market's `QuoterSlabV0` rode along.
    has_slab: bool,
}

pub fn handle_sync_liq_conditions<'c: 'info, 'info>(
    ctx: Context<'info, SyncLiqConditions<'info>>,
    args: SyncLiqConditionsArgs,
) -> Result<()> {
    let terms = SyncCaller {
        payer: &ctx.accounts.payer,
        state: &ctx.accounts.state,
        user: &ctx.accounts.user,
        conditions: &ctx.accounts.liq_conditions,
    }
    .authorized_terms(&args)?;
    // `stamp_sync_slot` is true because this sync brings the block up to date,
    // so a fresh account does not read as overdue from slot zero. The rewrite
    // stamps the slot itself, because a second load in the same instruction
    // cannot see a newly initialized account's discriminator.
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
    // A block written before the interval floor existed can still hold a
    // payment with a one-slot interval. Such terms are stored and armed as
    // unpaid, so the first rewrite repairs the block.
    let args = SyncLiqConditionsTerms {
        sync_payment_lamports: args.payable_lamports(),
        sync_fallback_slots: args.sync_fallback_slots,
    };
    let user_key = user_loader.key();
    refuse_duplicate_accounts(remaining_accounts)?;
    let inputs = collect_sync_inputs(remaining_accounts)?;
    let exposed_perps = validate_market_coverage(user_loader, &inputs.coverage())?;
    let oracle_refs = inputs
        .oracles
        .iter()
        .map(|key| AccountRefV0::readonly(key.to_bytes()))
        .collect();
    let sync_accounts = build_sync_accounts(
        liq_conditions.key(),
        user_key,
        oracle_refs,
        inputs.market_refs(),
        inputs.tail_refs,
    );

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
    // The resolver prefix and the margin map every condition on this account
    // points relay at. There is one stored list for all of them.
    let resolvers = conditions.write_sync_accounts(&sync_accounts)?;

    let user = crate::load!(user_loader)?;
    // Stamped before the conditions are armed, so a sync that arms none still
    // converges. The resolver compares digests rather than slot contents.
    conditions.positions_digest = UserConditionsV0::digest_positions(&user);
    if stamp_sync_slot {
        conditions.last_paid_sync_slot = Clock::get()?.slot;
    }

    arm_liveness_poll(&mut conditions, &inputs.perps, &exposed_perps, resolvers)?;
    arm_self_maintenance(&mut conditions, user_key, args, fallback_slots, resolvers)
}

/// The classified `remaining_accounts`. It holds the per-market inputs and the
/// account references a staged executor reuses.
struct SyncInputs {
    perps: BTreeMap<u16, MarketInputs>,
    spots: BTreeMap<u16, MarketInputs>,
    tail_refs: Vec<AccountRefV0>,
    oracles: BTreeSet<Pubkey>,
}

impl SyncInputs {
    /// Every spot market by index, then every perp market by index.
    ///
    /// `load_maps` reads spot markets only until the first perp market. The
    /// caller's order is therefore not stored. A perp market before the quote
    /// spot market would drop the quote market from every staged executor.
    fn market_refs(&self) -> Vec<AccountRefV0> {
        self.spots
            .values()
            .chain(self.perps.values())
            .filter_map(|inputs| inputs.market)
            .map(|key| AccountRefV0::writable(key.to_bytes()))
            .collect()
    }

    /// The view [`validate_market_coverage`] answers over.
    fn coverage(&self) -> MarketCoverage {
        MarketCoverage {
            perp_oracles: self
                .perps
                .iter()
                .filter_map(|(index, inputs)| Some((*index, inputs.oracle?)))
                .collect(),
            perp_quote_markets: self
                .perps
                .iter()
                .filter_map(|(index, inputs)| Some((*index, inputs.quote_spot_market_index?)))
                .collect(),
            spot_oracles: self
                .spots
                .iter()
                .filter_map(|(index, inputs)| Some((*index, inputs.oracle?)))
                .collect(),
            perp_books: self
                .perps
                .iter()
                .filter(|(_, inputs)| inputs.has_clob)
                .map(|(index, _)| *index)
                .collect(),
            perp_cranks: self
                .perps
                .iter()
                .filter(|(_, inputs)| inputs.crank_payment.is_some())
                .map(|(index, _)| *index)
                .collect(),
            perp_slabs: self
                .perps
                .iter()
                .filter(|(_, inputs)| inputs.has_slab)
                .map(|(index, _)| *index)
                .collect(),
            oracles: self.oracles.clone(),
        }
    }
}

/// Sort `remaining_accounts` by account type into market inputs and the
/// reference lists. Anything that is not a market or a crank account is a
/// candidate oracle. [`validate_market_coverage`] later refuses a candidate
/// that no passed market names.
fn collect_sync_inputs<'info>(
    remaining_accounts: &'info [AccountInfo<'info>],
) -> Result<SyncInputs> {
    let mut perps: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut spots: BTreeMap<u16, MarketInputs> = BTreeMap::new();
    let mut tail_refs: Vec<AccountRefV0> = Vec::new();
    let mut oracles: BTreeSet<Pubkey> = BTreeSet::new();
    let mut slab_books: BTreeSet<Pubkey> = BTreeSet::new();

    for info in remaining_accounts {
        if info.owner == &crate::ID {
            if let Ok(loader) = AccountLoader::<PerpMarket>::try_from(info) {
                let market = loader.load()?;
                let entry = perps.entry(market.market_index).or_default();
                entry.market = Some(*info.key);
                entry.oracle = Some(market.oracle);
                entry.quote_spot_market_index = Some(market.quote_spot_market_index);
                entry.has_clob = market.clob_market != Pubkey::default();
                continue;
            }
            if let Ok(loader) = AccountLoader::<SpotMarket>::try_from(info) {
                let market = loader.load()?;
                let entry = spots.entry(market.market_index).or_default();
                entry.market = Some(*info.key);
                entry.oracle = Some(market.oracle);
                continue;
            }
            if let Ok(loader) = AccountLoader::<ClobCrankConditionsV0>::try_from(info) {
                let conditions = loader.load()?;
                perps
                    .entry(conditions.market_index)
                    .or_default()
                    .crank_payment = Some(CrankPayment {
                    liquidation: u64::from(conditions.crank_payments.liquidation),
                    force_cancel: u64::from(conditions.crank_payments.force_cancel),
                });

                tail_refs.push(AccountRefV0::readonly(info.key.to_bytes()));
                continue;
            }

            // A quoter slab is the trigger pass's input rather than this
            // pass's. This pass writes the shared account list that both
            // passes' staged executors reuse, so a slab must stay out of the
            // map section. `load_maps` stops at the first non-map account, so
            // a slab filed among the oracles cuts the markets off from every
            // staged executor. Storing slabs after the markets, where the
            // parser never reaches, keeps them available to a staged resync.
            if let Ok(loader) = AccountLoader::<QuoterSlabV0>::try_from(info) {
                perps.entry(loader.load()?.market).or_default().has_slab = true;
                tail_refs.push(AccountRefV0::readonly(info.key.to_bytes()));
                // The book and its program ride with the slab. The resolver
                // reads the book to name the makers a liquidation fill settles
                // against, and cannot reach an account the stored list omits.
                // The slab names both, so it always carries the book.
                let slots = loader.slots()?;
                if let Some(index) = crate::state::prop_amm::clob_slot_index(&slots) {
                    let config = &slots[index].config;
                    tail_refs.push(AccountRefV0::writable(config.response_account.to_bytes()));
                    tail_refs.push(AccountRefV0::readonly(config.program_id.to_bytes()));
                    slab_books.extend([config.response_account, config.program_id]);
                }

                continue;
            }

            // Anything else velocity-owned falls through to the oracle
            // candidates below. Velocity hosts its own oracle accounts, such
            // as PythLazer and prelaunch, and those must land in the map
            // section.
        }

        oracles.insert(*info.key);
    }

    // A resync replays the stored list, which carries each slab's book and
    // program. The slab stores them in the tail already, so neither is an
    // oracle candidate.
    oracles.retain(|key| !slab_books.contains(key));
    Ok(SyncInputs {
        perps,
        spots,
        tail_refs,
        oracles,
    })
}

/// Every market the user has exposure in has to be present.
///
/// Both entry points are permissionless, and the maps come from whatever
/// accounts the caller passed. A caller that passes fewer markets than the
/// user holds writes a partial stored account list, and one that passes none
/// writes an empty one. The staged resolvers read their account list back out
/// of that stored list, so an empty one makes the liquidation resolver load no
/// markets and fail. The staged repair inherits it and cannot recover.
///
/// The emptiness test is the margin engine's own `is_available` rather than a
/// non-zero base amount. A position that carries only open orders, unsettled
/// PnL or an isolated balance is one the margin walk still visits.
///
/// A market entry is usable only if its oracle account rode along. The
/// resolver reads the price off that account, so without it every wake fails
/// and the block reads healthy while the user is never liquidated. This also
/// stops a caller from omitting an oracle to shield a user. A perp entry filed
/// only from a `ClobCrankConditionsV0` sets no oracle, so the `PerpMarket`
/// itself must be present. The quote market's default oracle needs no account.
///
/// Each exposed perp market also needs its quote spot market, because the
/// margin calculation loads it for every perp position.
///
/// A market with a CLOB must also bring its crank conditions account, which
/// prices the liveness poll, and its quoter slab, without which a staged
/// liquidation cannot sweep the user's book orders.
///
/// `load_maps` stops at the first account it does not recognize, so every
/// oracle candidate must be the oracle of a passed market.
///
/// Returns the perp markets the user has exposure in, which the liveness poll
/// is priced over.
pub fn validate_market_coverage(
    user_loader: &AccountLoader<'_, User>,
    coverage: &MarketCoverage,
) -> Result<Vec<u16>> {
    let oracle_present = |oracle: &Pubkey| -> bool {
        *oracle == Pubkey::default() || coverage.oracles.contains(oracle)
    };
    coverage.refuse_unnamed_oracles()?;

    let user = crate::load!(user_loader)?;
    let mut exposed_perps: Vec<u16> = Vec::new();
    for position in user.perp_positions.iter() {
        if position.is_available() {
            continue;
        }

        let index = position.market_index;
        validate!(
            coverage.perp_oracles.contains_key(&index),
            ErrorCode::InvalidUserConditionsSync,
            "sync is missing perp market {}, which the user has exposure in",
            index
        )?;
        validate!(
            oracle_present(&coverage.perp_oracles[&index]),
            ErrorCode::InvalidUserConditionsSync,
            "sync is missing the oracle account for perp market {}",
            index
        )?;
        validate!(
            coverage
                .perp_quote_markets
                .get(&index)
                .is_some_and(|quote| coverage.spot_oracles.contains_key(quote)),
            ErrorCode::InvalidUserConditionsSync,
            "sync is missing the quote spot market of perp market {}",
            index
        )?;
        validate!(
            !coverage.perp_books.contains(&index) || coverage.perp_cranks.contains(&index),
            ErrorCode::InvalidUserConditionsSync,
            "sync is missing the crank conditions account for perp market {}",
            index
        )?;
        validate!(
            !coverage.perp_books.contains(&index) || coverage.perp_slabs.contains(&index),
            ErrorCode::InvalidUserConditionsSync,
            "sync is missing the quoter slab for perp market {}",
            index
        )?;

        exposed_perps.push(index);
    }
    for position in user.spot_positions.iter() {
        if position.is_available() {
            continue;
        }

        let index = position.market_index;
        validate!(
            coverage.spot_oracles.contains_key(&index),
            ErrorCode::InvalidUserConditionsSync,
            "sync is missing spot market {}, which the user has exposure in",
            index
        )?;
        validate!(
            oracle_present(&coverage.spot_oracles[&index]),
            ErrorCode::InvalidUserConditionsSync,
            "sync is missing the oracle account for spot market {}",
            index
        )?;
    }

    Ok(exposed_perps)
}

/// The markets and oracles one sync pass was handed, reduced to what the
/// coverage rule reads. Both passes write the same shared account list and
/// answer this question over it, so the weaker pass cannot replace a complete
/// list with a partial one for a user it does not control.
pub struct MarketCoverage {
    /// Oracle each given perp market names. A market absent here had no
    /// `PerpMarket` account in the call.
    pub perp_oracles: BTreeMap<u16, Pubkey>,
    /// Quote spot market each given perp market names.
    pub perp_quote_markets: BTreeMap<u16, u16>,
    pub spot_oracles: BTreeMap<u16, Pubkey>,
    /// Given perp markets with a CLOB attached.
    pub perp_books: BTreeSet<u16>,
    /// Perp markets whose crank conditions account rode along.
    pub perp_cranks: BTreeSet<u16>,
    /// Perp markets whose quoter slab rode along.
    pub perp_slabs: BTreeSet<u16>,
    /// Oracle accounts the call carried.
    pub oracles: BTreeSet<Pubkey>,
}

impl MarketCoverage {
    /// Refuse an oracle candidate that no passed market names. The quote
    /// market names the default key, which is the System Program and no
    /// oracle, so that name does not count.
    fn refuse_unnamed_oracles(&self) -> Result<()> {
        let named: BTreeSet<&Pubkey> = self
            .perp_oracles
            .values()
            .chain(self.spot_oracles.values())
            .filter(|oracle| **oracle != Pubkey::default())
            .collect();

        match self.oracles.iter().find(|oracle| !named.contains(oracle)) {
            Some(unnamed) => {
                msg!(
                    "sync account {} is not a market, a crank account or a market's oracle",
                    unnamed
                );
                Err(ErrorCode::InvalidUserConditionsSync.into())
            }
            None => Ok(()),
        }
    }
}

/// Refuse an account passed twice. A second copy of a market makes
/// `load_maps` fail over the stored list, so no staged executor could load it.
pub fn refuse_duplicate_accounts(remaining_accounts: &[AccountInfo]) -> Result<()> {
    let mut seen: BTreeSet<&Pubkey> = BTreeSet::new();
    match remaining_accounts
        .iter()
        .find(|info| !seen.insert(info.key))
    {
        Some(duplicate) => {
            msg!("sync account {} is passed twice", duplicate.key);
            Err(ErrorCode::InvalidUserConditionsSync.into())
        }
        None => Ok(()),
    }
}

/// The account list staged executors reuse, in `load_maps` order.
///
/// The stored list is also the conditions' indirect resolver list, so it leads
/// with the accounts `ResolveLiquidatePerpWithFill` names, in that struct's
/// declaration order. `read_sync_accounts` skips them.
fn build_sync_accounts(
    conditions_key: Pubkey,
    user_key: Pubkey,
    oracle_refs: Vec<AccountRefV0>,
    market_refs: Vec<AccountRefV0>,
    tail_refs: Vec<AccountRefV0>,
) -> Vec<AccountRefV0> {
    let mut sync_accounts = vec![
        AccountRefV0::writable(pdas::relay_scratch().to_bytes()),
        AccountRefV0::readonly(conditions_key.to_bytes()),
        AccountRefV0::readonly(user_key.to_bytes()),
        AccountRefV0::readonly(pdas::state().to_bytes()),
    ];

    sync_accounts.extend(oracle_refs);
    sync_accounts.extend(market_refs);
    sync_accounts.extend(tail_refs);
    sync_accounts
}

/// Arm the poll that wakes `ResolveLiquidatePerpWithFill`.
///
/// The poll predicts nothing. It wakes on a clock and lets the resolver run
/// the real maintenance-margin calculation, which reports no work while the
/// account is healthy. That calculation needs the margin map, so the watch and
/// the fallback poll keep the stored account list current.
///
/// The poll is priced at the cheapest crank any market the user has exposure
/// in pays. Relay holds a keeper's balance growth to the floor a condition
/// advertises, so a floor above what the market pays fails the crank it asked
/// for. A force cancel comes before a liquidation on the same ladder and pays
/// the market's force-cancel figure, so the lower of the two figures is the
/// floor. A market the caller passed that the user has no exposure in says
/// nothing about what this account's liquidation pays.
fn arm_liveness_poll(
    conditions: &mut UserConditionsV0,
    perps: &BTreeMap<u16, MarketInputs>,
    exposed_perps: &[u16],
    resolvers: relay_spec::ResolverListV0,
) -> Result<()> {
    let poll_payment = exposed_perps
        .iter()
        .filter_map(|index| perps.get(index)?.crank_payment)
        .map(|payment| payment.liquidation.min(payment.force_cancel))
        .min()
        .unwrap_or(0);
    if poll_payment == 0 {
        // No market the user has exposure in has a reservoir to pay from, so
        // the poll would advertise work nobody is paid for. A turner filters
        // out a condition that pays nothing. The signed keeper path still
        // liquidates this account.
        return conditions
            .set_condition(LIQ_LIVENESS_POLL, &relay_spec::bytemuck::Zeroable::zeroed());
    }

    conditions.set_condition(
        LIQ_LIVENESS_POLL,
        &ConditionV0::every_slots(
            LIQ_LIVENESS_POLL_SLOTS,
            CrankSpecV0 {
                resolver_program: crate::ID.to_bytes(),
                resolver_disc: crate::instructions::relay_harness::disc8(
                    crate::instruction::ResolveLiquidatePerpWithFill::DISCRIMINATOR,
                )?,

                min_payment: poll_payment,
            },
            resolvers,
        ),
    )
}

/// Arm the block's self-maintenance. A change to the user's own position bytes
/// rewrites the block, and a coarse poll catches whatever that watch misses.
fn arm_self_maintenance(
    conditions: &mut UserConditionsV0,
    user_key: Pubkey,
    args: SyncLiqConditionsTerms,
    fallback_slots: u64,
    resolvers: relay_spec::ResolverListV0,
) -> Result<()> {
    if args.sync_payment_lamports > 0 {
        let sync_spec = CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc: crate::instructions::relay_harness::disc8(
                crate::instruction::ResolveResyncLiqConditions::DISCRIMINATOR,
            )?,

            min_payment: args.sync_payment_lamports,
        };
        let (watch_offset, watch_len) = user_positions_watch_region();
        conditions.set_condition(
            LIQ_SYNC_WATCH,
            &ConditionV0::on_account_change(
                relay_spec::WatchedRegion::new(user_key.to_bytes(), watch_offset, watch_len),
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

/// The bounds that can be read off the arguments alone.
///
/// The interval floor is not here. It binds the payment a block ends up
/// holding rather than the cost units a caller names, so it lives on
/// [`SyncLiqConditionsTerms`] and is checked once the terms are priced.
pub fn validate_sync_args(args: &SyncLiqConditionsArgs) -> Result<()> {
    // The treasury pays this figure to whoever cranks the resync, and opting
    // in is permissionless. Without a ceiling a caller could name its own
    // price against protocol funds and collect it by cranking itself.
    validate!(
        args.sync_cost_units <= crate::state::user_conditions::LIQ_SYNC_MAX_COST_UNITS,
        ErrorCode::SelfSyncCostAboveCeiling,
        "self-sync priced at {} cost units, above the {} ceiling",
        args.sync_cost_units,
        crate::state::user_conditions::LIQ_SYNC_MAX_COST_UNITS
    )?;

    // The interval must also stay short enough for the poll to be a safety
    // net. Opting in is permissionless, so this bounds what a third party can
    // do to an account it does not control.
    validate!(
        args.sync_fallback_slots <= crate::state::user_conditions::LIQ_SYNC_MAX_FALLBACK_SLOTS,
        ErrorCode::SelfSyncIntervalAboveCeiling,
        "a self-sync interval of {} slots is above the {} ceiling",
        args.sync_fallback_slots,
        crate::state::user_conditions::LIQ_SYNC_MAX_FALLBACK_SLOTS
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::{refuse_duplicate_accounts, validate_market_coverage, MarketCoverage},
        crate::{
            create_anchor_account_info,
            state::user::{PerpPosition, User},
        },
        anchor_lang::prelude::{AccountInfo, AccountLoader, Pubkey},
        std::collections::{BTreeMap, BTreeSet},
    };

    const ORACLE: Pubkey = Pubkey::new_from_array([7; 32]);

    /// A user long on perp market 0, and the coverage of a call that carried
    /// that market, its oracle, its crank account and its slab.
    fn full_coverage() -> MarketCoverage {
        MarketCoverage {
            perp_oracles: BTreeMap::from([(0, ORACLE)]),
            perp_quote_markets: BTreeMap::from([(0, 0)]),
            spot_oracles: BTreeMap::from([(0, Pubkey::default())]),
            perp_books: BTreeSet::from([0]),
            perp_cranks: BTreeSet::from([0]),
            perp_slabs: BTreeSet::from([0]),
            oracles: BTreeSet::from([ORACLE]),
        }
    }

    fn check(coverage: &MarketCoverage) -> bool {
        let mut user = User::default();
        user.perp_positions[0] = PerpPosition {
            market_index: 0,
            base_asset_amount: 1,
            ..PerpPosition::default()
        };

        create_anchor_account_info!(user, User, user_account_info);
        let user_loader = AccountLoader::<User>::try_from(&user_account_info).unwrap();
        validate_market_coverage(&user_loader, coverage).is_ok()
    }

    #[test]
    fn a_full_call_is_accepted() {
        assert!(check(&full_coverage()));
    }

    /// The System Program sorts first among the oracles, and `load_maps`
    /// stops there, so an account no market names must be refused.
    #[test]
    fn an_account_no_market_names_is_refused() {
        let mut coverage = full_coverage();
        coverage.oracles.insert(Pubkey::default());
        assert!(!check(&coverage));
    }

    /// Without the slab the stored list names no book, and a staged
    /// liquidation can never sweep the user's book orders.
    #[test]
    fn a_clob_market_without_its_slab_is_refused() {
        let mut coverage = full_coverage();
        coverage.perp_slabs.clear();
        assert!(!check(&coverage));
    }

    /// The margin calculation loads the quote spot market of every perp
    /// position, so a list without it fails every staged executor.
    #[test]
    fn a_perp_market_without_its_quote_spot_market_is_refused() {
        let mut coverage = full_coverage();
        coverage.spot_oracles.clear();
        assert!(!check(&coverage));
    }

    #[test]
    fn an_account_passed_twice_is_refused() {
        let key = Pubkey::new_unique();
        let owner = Pubkey::default();
        let (mut first_lamports, mut second_lamports) = (0, 0);
        let (mut first_data, mut second_data) = (vec![], vec![]);
        let first = AccountInfo::new(
            &key,
            false,
            false,
            &mut first_lamports,
            &mut first_data,
            &owner,
            false,
        );
        let second = AccountInfo::new(
            &key,
            false,
            false,
            &mut second_lamports,
            &mut second_data,
            &owner,
            false,
        );

        assert!(refuse_duplicate_accounts(std::slice::from_ref(&first)).is_ok());
        assert!(refuse_duplicate_accounts(&[first, second]).is_err());
    }

    /// `load_maps` reads spot markets only until the first perp market. A call
    /// that passes the perp market first must still store the quote spot
    /// market where the parser reads it.
    #[test]
    fn markets_are_stored_spot_first_whatever_the_call_order() {
        use {
            super::collect_sync_inputs,
            crate::{
                instructions::optional_accounts::load_maps,
                math::time::SlotClock,
                state::{
                    perp_market::PerpMarket, perp_market_map::MarketSet, spot_market::SpotMarket,
                },
            },
        };

        let (perp_key, spot_key) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut perp = PerpMarket::default();
        create_anchor_account_info!(perp, &perp_key, PerpMarket, perp_info);
        let mut spot = SpotMarket::default();
        create_anchor_account_info!(spot, &spot_key, SpotMarket, spot_info);

        let passed = [perp_info.clone(), spot_info.clone()];
        let inputs = collect_sync_inputs(&passed).unwrap();
        let stored: Vec<Pubkey> = inputs
            .market_refs()
            .iter()
            .map(|account| Pubkey::new_from_array(account.address))
            .collect();
        assert_eq!(stored, vec![spot_key, perp_key]);

        let replayed = [spot_info, perp_info];
        let maps = load_maps(
            &mut replayed.iter().peekable(),
            &MarketSet::new(),
            &MarketSet::new(),
            0,
            SlotClock::baseline(),
            None,
        )
        .unwrap();
        assert!(maps.spot_market_map.get_ref(&0).is_ok());
        assert!(maps.perp_market_map.get_ref(&0).is_ok());
    }

    /// A resync replays the list a sync stored, which carries the slab's book
    /// and program. The replay must classify to the same list.
    #[test]
    fn a_stored_list_replays_to_the_same_list() {
        use {
            super::{build_sync_accounts, collect_sync_inputs, SyncInputs},
            crate::state::{
                clob_crank::ClobCrankConditionsV0,
                perp_market::PerpMarket,
                prop_amm::{QuoterSlabV0, QuoterSlotV0, QuoterType},
            },
            anchor_lang::Discriminator,
            relay_spec::AccountRefV0,
        };

        let (book_key, program_key) = (Pubkey::new_unique(), Pubkey::new_unique());
        let (market_key, crank_key) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut market = PerpMarket {
            oracle: ORACLE,
            clob_market: book_key,
            ..PerpMarket::default()
        };
        create_anchor_account_info!(market, &market_key, PerpMarket, market_info);
        let mut crank = ClobCrankConditionsV0::default();
        create_anchor_account_info!(crank, &crank_key, ClobCrankConditionsV0, crank_info);

        let mut slot: QuoterSlotV0 = bytemuck::Zeroable::zeroed();
        slot.entry = Pubkey::new_unique();
        slot.config.quoter_type = QuoterType::Clob;
        slot.config.response_account = book_key;
        slot.config.program_id = program_key;
        let header = QuoterSlabV0 {
            capacity: 1,
            ..QuoterSlabV0::default()
        };
        let mut slab_data = QuoterSlabV0::DISCRIMINATOR.to_vec();
        slab_data.extend_from_slice(bytemuck::bytes_of(&header));
        slab_data.extend_from_slice(bytemuck::bytes_of(&slot));

        let (slab_key, owner, other_owner) =
            (Pubkey::new_unique(), crate::ID, Pubkey::new_unique());
        let mut lamports = [0u64; 4];
        let [slab_lamports, oracle_lamports, book_lamports, program_lamports] = &mut lamports;
        let (mut oracle_data, mut book_data, mut program_data) = (vec![], vec![], vec![]);
        let slab_info = AccountInfo::new(
            &slab_key,
            false,
            false,
            slab_lamports,
            &mut slab_data,
            &owner,
            false,
        );
        let oracle_info = AccountInfo::new(
            &ORACLE,
            false,
            false,
            oracle_lamports,
            &mut oracle_data,
            &other_owner,
            false,
        );
        let book_info = AccountInfo::new(
            &book_key,
            false,
            true,
            book_lamports,
            &mut book_data,
            &other_owner,
            false,
        );
        let program_info = AccountInfo::new(
            &program_key,
            false,
            false,
            program_lamports,
            &mut program_data,
            &other_owner,
            true,
        );

        let stored = |inputs: SyncInputs| -> Vec<AccountRefV0> {
            inputs.coverage().refuse_unnamed_oracles().unwrap();
            let oracle_refs = inputs
                .oracles
                .iter()
                .map(|key| AccountRefV0::readonly(key.to_bytes()))
                .collect();
            build_sync_accounts(
                Pubkey::default(),
                Pubkey::default(),
                oracle_refs,
                inputs.market_refs(),
                inputs.tail_refs,
            )
        };

        let synced = [
            oracle_info.clone(),
            market_info.clone(),
            crank_info.clone(),
            slab_info.clone(),
        ];
        let first = stored(collect_sync_inputs(&synced).unwrap());
        let replayed = [
            oracle_info,
            market_info,
            crank_info,
            slab_info,
            book_info,
            program_info,
        ];
        let second = stored(collect_sync_inputs(&replayed).unwrap());

        assert_eq!(first.len(), 4 + 6);
        assert_eq!(first, second);
    }
}
