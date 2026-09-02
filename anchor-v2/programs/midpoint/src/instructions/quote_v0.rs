use {
    crate::state::{MidpointQuoterV0, ResponsePointerV0, UserRefV0},
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct QuoteV0 {
    /// mut only for the response tail — the ladder is not touched.
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
}

/// Declared by `quoter-spec`, which owns every shape on this wire. A local
/// mirror is not a convenience here: the fields are read in declaration
/// order, so a mirror that is missing one reads every field after it from the
/// wrong place and reports nothing wrong.
///
/// The midpoint reads `users`, `taker`, `limit_price` and `reference_price`.
/// It ignores `caps`: it settles against one standing-intent user and holds no
/// orders, so there is no per-user budget to spend and nothing to skip
/// mid-book. `reference_price` is velocity's oracle; the quoter refuses to
/// quote when its mid is outside the configured band of that price.
pub use quoter_spec::QuoteArgsV0;

/// Whether this quoter has anything to say to this caller: settleability,
/// self-trade, and (when configured) protected flow. The mid-staleness /
/// pause gate lives in `MidpointQuoterV0::is_quoting`, applied by the
/// quote/fill walks themselves.
///
/// The protected-flow branch reads `taker_served_window` off the wire:
/// velocity asserts that the flow served a protection window — the swift
/// hold (velocity read the co-signature off the instructions sysvar), or
/// the book's activation delay (a crank fills an order that rested through
/// it). The claim is trusted the way `users` and `caps` are: this program
/// already authenticates its caller, and the caller is the settlement
/// engine.
pub fn caller_gate(
    quoter: &MidpointQuoterV0,
    users: &[UserRefV0],
    taker: Option<&UserRefV0>,
    taker_served_window: bool,
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
    Ok(true)
}

/// Quoter interface: price levels for a taker of `direction`/`size` off the
/// spline at the current mid, streamed into the quoter's response tail; the
/// returned pointer locates them.
pub fn handle_quote_v0(ctx: &mut Context<QuoteV0>, args: QuoteArgsV0) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    let open = caller_gate(
        &ctx.accounts.quoter,
        args.users,
        args.taker.as_ref(),
        args.taker_served_window,
    )?;
    // A mid outside the band of velocity's oracle quotes nothing, so a
    // compromised hot key cannot draw flow onto an off-market price.
    let open = open
        && ctx
            .accounts
            .quoter
            .mid_within_deviation(args.reference_price);
    ctx.accounts.quoter.write_quote_response(
        args.direction,
        args.size,
        args.limit_price,
        clock.slot,
        open,
    )
}
