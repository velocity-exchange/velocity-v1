use {
    crate::{
        error::MidpointError,
        introspection::tx_co_signed_by,
        state::{Direction, MidpointQuoterV0, QuoteResponseV0, ResponsePointerV0, UserRefV0},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct QuoteV0 {
    /// mut only for the response tail — the ladder is not touched.
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    /// CHECK: the instructions sysvar; `Instructions::try_from` inside the
    /// attestation check verifies the address, and it is only read when the
    /// quoter requires attested flow.
    pub instructions_sysvar: UncheckedAccount,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct QuoteArgsV0 {
    pub direction: Direction,
    pub size: u64,
    /// `User`s the caller can settle balance changes for. The midpoint
    /// settles against exactly one user — if it is absent from a `Some`
    /// set, the book is empty (not an error: the caller simply can't
    /// settle us, so we have nothing for them).
    pub users: Option<Vec<UserRefV0>>,
    /// The taker's `User`: quoting yourself is a wash trade, so the quoted
    /// user's own flow sees an empty book.
    pub taker: Option<UserRefV0>,
}

/// Whether this quoter has anything to say to this caller: settleability,
/// self-trade, and (when configured) flow attestation. The mid-staleness /
/// pause gate lives in `MidpointQuoterV0::is_quoting`, applied by the
/// quote/fill walks themselves.
pub fn caller_gate(
    quoter: &MidpointQuoterV0,
    users: Option<&[UserRefV0]>,
    taker: Option<&UserRefV0>,
    instructions_sysvar: &anchor_lang_v2::pinocchio::account::AccountView,
) -> Result<bool> {
    let quoted = quoter.user_ref();
    if users.is_some_and(|set| !set.contains(&quoted)) {
        return Ok(false);
    }
    if taker.is_some_and(|taker| *taker == quoted) {
        return Ok(false);
    }
    if quoter.require_attested_flow != 0
        && !tx_co_signed_by(instructions_sysvar, &quoter.flow_authority)?
    {
        return Ok(false);
    }
    Ok(true)
}

/// Quoter interface: price levels for a taker of `direction`/`size` off the
/// spline at the current mid, written to the quoter's response tail; the
/// returned pointer locates them.
pub fn handle_quote_v0(ctx: &mut Context<QuoteV0>, args: QuoteArgsV0) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    let open = caller_gate(
        &ctx.accounts.quoter,
        args.users.as_deref(),
        args.taker.as_ref(),
        ctx.accounts.instructions_sysvar.account(),
    )?;
    let quoter = &mut ctx.accounts.quoter;
    let levels = if open {
        quoter.quote(args.direction, args.size, clock.slot)
    } else {
        Vec::new()
    };

    let mut data = Vec::with_capacity(1024);
    anchor_lang_v2::wincode::config::serialize_into(
        &mut data,
        &QuoteResponseV0 { levels },
        anchor_lang_v2::BORSH_CONFIG,
    )
    .map_err(|_| MidpointError::ResponseTooLarge)?;
    quoter.write_response(&data)
}
