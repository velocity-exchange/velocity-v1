use {
    crate::state::{MidpointQuoterV0, ResponsePointerV0, UserRefV0},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct QuoteV0 {
    /// Mutable only for the response tail. The ladder is not written.
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
}

/// allow-verbose: declared by `quoter-spec`, which owns this wire's shape.
/// A local mirror is unsafe, since the reader takes fields in declaration
/// order and a missing field silently reads everything after it wrong.
///
/// The midpoint reads `users`, `taker`, `limit_price`, and `reference_price`,
/// and ignores `caps`: it settles one standing-intent user and holds no
/// orders, so there is no per-user budget to spend or book to skip mid-way.
/// `reference_price` is velocity's oracle price, and the quoter refuses to
/// quote when its mid is outside the configured band of that price. A quote
/// with no reference price is a read, and skips the band.
pub use quoter_spec::QuoteArgsV0;

/// Report whether this quoter has anything to say to this caller. The gate
/// covers settleability, self-trade, protected flow when configured, and the
/// mid's distance from velocity's oracle price. A mid outside the band
/// answers nothing, so a compromised hot key cannot draw flow onto an
/// off-market price. Staleness and pause live in `is_quoting`.
///
/// Velocity asserts `taker_served_window`, meaning the flow served the swift
/// hold or the book's activation delay. This program trusts that claim as it
/// trusts `users` and `caps`, because it authenticates its caller and the
/// caller is the settlement engine.
pub fn is_open(
    quoter: &MidpointQuoterV0,
    users: &[UserRefV0],
    taker: Option<&UserRefV0>,
    taker_served_window: bool,
    reference_price: Option<u64>,
) -> Result<bool> {
    // The caps address a user by its index in this set, and the exclusion
    // bitmap holds one bit per slot up to the capacity. A longer set carries
    // users no bit can exclude.
    require!(
        crate::state::user_set_within_capacity(users),
        crate::error::MidpointError::OversizedUserSet
    );

    let quoted = quoter.user_ref();
    if !users.is_empty() && !users.contains(&quoted) {
        return Ok(false);
    }
    if taker.is_some_and(|taker| *taker == quoted) {
        return Ok(false);
    }
    if quoter.require_attested_flow != 0 && !taker_served_window {
        return Ok(false);
    }

    Ok(reference_price.is_none_or(|price| quoter.mid_within_deviation(price)))
}

/// Quoter interface. Price the levels a taker of `direction` and `size` takes
/// off the spline at the current mid. The handler streams them into the
/// quoter's response tail, and the returned pointer locates them.
pub fn handle_quote_v0(ctx: &mut Context<QuoteV0>, args: QuoteArgsV0) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    let open = is_open(
        &ctx.accounts.quoter,
        args.users,
        args.taker.as_ref(),
        args.taker_served_window,
        args.reference_price,
    )?;

    ctx.accounts.quoter.write_quote_response(
        args.direction,
        args.size,
        args.limit_price,
        clock.slot,
        open,
    )
}
