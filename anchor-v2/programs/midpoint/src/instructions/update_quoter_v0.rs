use {
    crate::{error::MidpointError, state::MidpointQuoterV0},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct UpdateQuoterV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    /// The maker's config key. It is the only key that can reconfigure the
    /// instance, and no instruction reassigns it, so the maker keeps the pause
    /// control. It is not the quoted wallet. The quoted user seeds the PDA and
    /// is fixed at creation, so no config update can send fills to a different
    /// `User`.
    #[account(address = quoter.authority @ MidpointError::InvalidAuthority)]
    pub authority: Signer,
    /// When present, this account becomes the new hot key.
    pub new_hot_authority: Option<UncheckedAccount>,
}

#[derive(Clone, Default, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct UpdateQuoterArgsV0 {
    pub max_mid_staleness_slots: Option<u64>,
    pub price_tick_size: Option<u64>,
    pub size_step: Option<u64>,
    pub min_quote_size: Option<u64>,
    /// Quote only flow that served a protection window. The claim arrives on
    /// the wire as `taker_served_window`, and this program trusts its caller
    /// for it the way it trusts `users` and `caps`. Enabling it configures no
    /// flow key here and reads no velocity account.
    pub require_attested_flow: Option<bool>,
    pub is_paused: Option<bool>,
    /// The maximum mid deviation from velocity's oracle, in parts per million.
    /// A value of 0 disables the bound, and only the config key can choose
    /// that. Creation refuses 0, so an instance never starts without the
    /// bound.
    pub max_mid_deviation_ppm: Option<u64>,
    /// New value for the racing-writer sequence counter. Any value is legal,
    /// including a lower one.
    ///
    /// The counter only rises through `set_mid_v0`. A writer that sends
    /// `u64::MAX` by mistake makes every later mid write fail for the life of
    /// the instance. This field is the recovery, and only the config key can
    /// set it. A reset re-opens the sequences below the old value, so the maker
    /// must stop the racing writers first.
    pub mid_sequence: Option<u64>,
}

pub fn handle_update_quoter_v0(
    ctx: &mut Context<UpdateQuoterV0>,
    args: UpdateQuoterArgsV0,
) -> Result<()> {
    let new_hot = ctx
        .accounts
        .new_hot_authority
        .as_ref()
        .map(|account| *account.address());
    let quoter = &mut ctx.accounts.quoter;
    if let Some(hot) = new_hot {
        quoter.hot_authority = hot;
    }
    if let Some(v) = args.max_mid_staleness_slots {
        quoter.max_mid_staleness_slots = v;
    }
    if let Some(v) = args.price_tick_size {
        quoter.price_tick_size = v;
    }
    if let Some(v) = args.size_step {
        quoter.size_step = v;
    }
    if let Some(v) = args.min_quote_size {
        quoter.min_quote_size = v;
    }
    if let Some(v) = args.require_attested_flow {
        quoter.require_attested_flow = v as u8;
    }
    if let Some(v) = args.is_paused {
        quoter.is_paused = v as u8;
    }
    if let Some(v) = args.max_mid_deviation_ppm {
        quoter.max_mid_deviation_ppm = v;
    }
    if let Some(v) = args.mid_sequence {
        quoter.mid_sequence = v;
    }
    quoter.validate()
}
