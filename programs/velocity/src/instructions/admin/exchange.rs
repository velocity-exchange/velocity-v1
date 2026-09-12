//! Exchange-wide settings on the `State` account.
//!
//! [`handle_initialize`] creates the account and writes every field once. The
//! other handlers each move one setting: the status the exchange runs under,
//! the mints it recognises, the auction and settlement durations, the oracle
//! guard rails, and the account limits.

use super::*;

pub fn handle_initialize(ctx: Context<Initialize>) -> Result<()> {
    let (velocity_signer, velocity_signer_nonce) =
        Pubkey::find_program_address(&[b"velocity_signer".as_ref()], ctx.program_id);

    let mut state = ctx.accounts.state.load_init()?;
    *state = initial_state(
        *ctx.accounts.admin.key,
        velocity_signer,
        velocity_signer_nonce,
    );

    Ok(())
}

/// The exchange at birth.
///
/// Every field is named, because a zero-copy account has no default to fall
/// back on. `warm_admin` starts as the cold admin, so the warm-tier handlers
/// work at once. Every hot role starts unassigned, and `update_hot_admin`
/// rotates one in.
fn initial_state(admin: Pubkey, velocity_signer: Pubkey, velocity_signer_nonce: u8) -> State {
    State {
        cold_admin: admin,
        warm_admin: admin,
        pause_admin: Pubkey::default(),
        hot_amm_crank: Pubkey::default(),
        hot_lp_cache: Pubkey::default(),
        hot_lp_swap: Pubkey::default(),
        hot_lp_settle: Pubkey::default(),
        hot_feature_flag: Pubkey::default(),
        hot_fuel: Pubkey::default(),
        hot_user_flag: Pubkey::default(),
        hot_vault_deposit: Pubkey::default(),
        hot_mm_oracle_crank: Pubkey::default(),
        hot_amm_spread_adjust: Pubkey::default(),
        exchange_status: ExchangeStatus::active(),
        whitelist_mint: Pubkey::default(),
        discount_mint: Pubkey::default(),
        oracle_guard_rails: OracleGuardRails::default(),
        number_of_authorities: 0,
        number_of_sub_accounts: 0,
        number_of_markets: 0,
        number_of_spot_markets: 0,
        min_perp_auction_duration: legacy_slot_duration_u8(10),
        default_market_order_time_in_force: 60,
        default_spot_auction_duration: 10,
        liquidation_margin_buffer_ratio: DEFAULT_LIQUIDATION_MARGIN_BUFFER_RATIO,
        settlement_duration: 0, // extra duration after market expiry to allow settlement
        signer: velocity_signer,
        signer_nonce: velocity_signer_nonce,
        srm_vault: Pubkey::default(),
        protocol_fee_recipient_perp: Pubkey::default(),
        hot_fee_withdraw: Pubkey::default(),
        hot_account_extension: Pubkey::default(),
        protocol_fee_recipient_spot: Pubkey::default(),
        perp_fee_structure: FeeStructure::perps_default(),
        spot_fee_structure: FeeStructure::spot_default(),
        liquidation_duration: legacy_slot_duration_u8(0),
        initial_pct_to_liquidate: 0,
        max_number_of_sub_accounts: 0,
        max_initialize_user_fee: 0,
        feature_bit_flags: 0,
        lp_pool_feature_bit_flags: 0,
        solvency_status: SolvencyStatus::active(),
        promo_fee_tier: 0,
        slot_duration_ms: 0,
        pending_slot_duration_ms: 0,
        slot_duration_pad: [0; 2],
        slot_duration_effective_slot: 0,
        slot_duration_transition_slots: [0; 4],
        hot_flow_authority: Pubkey::default(),
        transaction_fee_rails: TransactionFeeRails::FLAT_PER_SIGNATURE,
        // Five percent of a liquidation's value is the ceiling on what the
        // protocol will spend getting it cranked; see the field's docs. Kept
        // low so a keeper that also builds the block cannot bill much of the
        // recovery back as its own priority fee.
        liquidation_crank_reimbursement_bps: 500,
        // Set by the admin once a SOL spot market exists; until then the
        // liquidation crank pays its flat figure and nothing more.
        sol_spot_market_index: 0,
        padding_0: [0; 2],
        padding: [0; 142],
    }
}

/// Sane ceiling (in 400ms baseline units) on the margin oracle staleness window.
/// ~4.6 days of tolerance, absurd as a real config but far below the point
/// where `Millis::from_stored_units` would saturate; keeps a fat-fingered
/// value from turning the staleness gate into a global never-stale.
const MAX_STALENESS_STORED_UNITS: i64 = 1_000_000;

/// Tighter ceiling on the *AMM* staleness window: `get_oracle_status` routes it
/// through `oracle_validity`'s `i8` slot-delay override (it doubles as the AMM's
/// immediate/low-risk delay override), so a value above `i8::MAX` would pass this
/// setter but then `CastingFailure` in every funding path that calls
/// `get_oracle_status`. `i8::MAX` units is ~50s at the 400ms baseline, far above
/// any real AMM freshness window (default 10).
const MAX_AMM_STALENESS_STORED_UNITS: i64 = i8::MAX as i64;

pub fn handle_update_oracle_guard_rails(
    ctx: Context<AdminUpdateState>,
    oracle_guard_rails: OracleGuardRails,
) -> Result<()> {
    validate!(
        (0..=MAX_AMM_STALENESS_STORED_UNITS).contains(&legacy_slot_duration_i64_raw(
            oracle_guard_rails.validity.slots_before_stale_for_amm,
        )) && (0..=MAX_STALENESS_STORED_UNITS).contains(&legacy_slot_duration_i64_raw(
            oracle_guard_rails.validity.slots_before_stale_for_margin,
        )),
        ErrorCode::DefaultError,
        "oracle staleness windows out of range: amm [0, {}], margin [0, {}]",
        MAX_AMM_STALENESS_STORED_UNITS,
        MAX_STALENESS_STORED_UNITS
    )?;

    msg!(
        "oracle_guard_rails: {:?} -> {:?}",
        ctx.accounts.state.load()?.oracle_guard_rails,
        oracle_guard_rails
    );

    ctx.accounts.state.load_mut()?.oracle_guard_rails = oracle_guard_rails;
    Ok(())
}

pub fn handle_update_state_settlement_duration(
    ctx: Context<AdminUpdateState>,
    settlement_duration: u16,
) -> Result<()> {
    msg!(
        "settlement_duration: {} -> {}",
        ctx.accounts.state.load()?.settlement_duration,
        settlement_duration
    );

    ctx.accounts.state.load_mut()?.settlement_duration = settlement_duration;
    Ok(())
}

pub fn handle_update_state_max_number_of_sub_accounts(
    ctx: Context<AdminUpdateState>,
    max_number_of_sub_accounts: u16,
) -> Result<()> {
    msg!(
        "max_number_of_sub_accounts: {} -> {}",
        ctx.accounts.state.load()?.max_number_of_sub_accounts,
        max_number_of_sub_accounts
    );

    ctx.accounts.state.load_mut()?.max_number_of_sub_accounts = max_number_of_sub_accounts;
    Ok(())
}

pub fn handle_update_state_max_initialize_user_fee(
    ctx: Context<AdminUpdateState>,
    max_initialize_user_fee: u16,
) -> Result<()> {
    msg!(
        "max_initialize_user_fee: {} -> {}",
        ctx.accounts.state.load()?.max_initialize_user_fee,
        max_initialize_user_fee
    );

    ctx.accounts.state.load_mut()?.max_initialize_user_fee = max_initialize_user_fee;
    Ok(())
}

pub fn handle_update_whitelist_mint(
    ctx: Context<AdminUpdateState>,
    whitelist_mint: Pubkey,
) -> Result<()> {
    msg!(
        "whitelist_mint: {:?} -> {:?}",
        ctx.accounts.state.load()?.whitelist_mint,
        whitelist_mint
    );

    ctx.accounts.state.load_mut()?.whitelist_mint = whitelist_mint;
    Ok(())
}

pub fn handle_update_discount_mint(
    ctx: Context<AdminUpdateState>,
    discount_mint: Pubkey,
) -> Result<()> {
    msg!(
        "discount_mint: {:?} -> {:?}",
        ctx.accounts.state.load()?.discount_mint,
        discount_mint
    );

    ctx.accounts.state.load_mut()?.discount_mint = discount_mint;
    Ok(())
}

pub fn handle_update_exchange_status(
    ctx: Context<PauseAdminUpdateState>,
    exchange_status: u8,
) -> Result<()> {
    let signer = ctx.accounts.admin.key();
    let mut state = ctx.accounts.state.load_mut()?;
    require_pause_only_added(&signer, &state, state.exchange_status, exchange_status)?;
    msg!(
        "exchange_status: {:?} -> {:?}",
        state.exchange_status,
        exchange_status
    );
    state.exchange_status = exchange_status;
    Ok(())
}

pub fn handle_update_solvency_status(
    ctx: Context<ColdAdminUpdateState>,
    solvency_status: u8,
) -> Result<()> {
    let mut state = ctx.accounts.state.load_mut()?;
    msg!(
        "solvency_status: {:?} -> {:?}",
        state.solvency_status,
        solvency_status
    );
    state.solvency_status = solvency_status;
    Ok(())
}

pub fn handle_update_perp_auction_duration(
    ctx: Context<AdminUpdateState>,
    min_perp_auction_duration: u8,
) -> Result<()> {
    msg!(
        "min_perp_auction_duration: {:?} -> {:?}",
        ctx.accounts.state.load()?.min_perp_auction_duration,
        min_perp_auction_duration
    );

    ctx.accounts.state.load_mut()?.min_perp_auction_duration =
        legacy_slot_duration_u8(min_perp_auction_duration);
    Ok(())
}

pub fn handle_update_spot_auction_duration(
    ctx: Context<AdminUpdateState>,
    default_spot_auction_duration: u8,
) -> Result<()> {
    msg!(
        "default_spot_auction_duration: {:?} -> {:?}",
        ctx.accounts.state.load()?.default_spot_auction_duration,
        default_spot_auction_duration
    );

    ctx.accounts.state.load_mut()?.default_spot_auction_duration = default_spot_auction_duration;
    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    // Only the designated `state_init_authority` may create the singleton
    // `State` account, so the one-time init cannot be front-run. This lock is
    // active *only* on a real mainnet build (`mainnet-beta` on, `anchor-test`
    // off). Every other build leaves `initialize` open to any admin key:
    //   - devnet/localnet (`mainnet-beta` off) — free setup, and
    //   - the integration-test build, which keeps the default `mainnet-beta`
    //     feature on but adds `anchor-test`, so each test's bankrun wallet can
    //     still initialize.
    // The two arms below are exact complements, so exactly one applies per
    // build. This mirrors the three-way build split used by the keys in
    // `ids.rs`.
    //
    // Anchor honors a single `#[account]` per field, so `mut` is repeated in
    // both arms rather than shared.
    #[cfg_attr(
        any(not(feature = "mainnet-beta"), feature = "anchor-test"),
        account(mut)
    )]
    #[cfg_attr(
        all(feature = "mainnet-beta", not(feature = "anchor-test")),
        account(mut, address = crate::ids::state_init_authority::id())
    )]
    pub admin: Signer<'info>,
    #[account(
        init,
        seeds = [b"velocity_state".as_ref()],
        space = State::SIZE,
        bump,
        payer = admin
    )]
    pub state: AccountLoader<'info, State>,
    pub quote_asset_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: checked in `initialize`
    pub velocity_signer: UncheckedAccount<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
    pub token_program: Interface<'info, TokenInterface>,
}
