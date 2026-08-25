use {
    crate::{
        introspection::tx_co_signed_by,
        state::{MidpointQuoterV0, ResponsePointerV0, UserRefV0},
        velocity::{hot_flow_authority, VELOCITY_STATE},
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
    /// CHECK: velocity's global `State`, address-locked. Read only for
    /// `hot_flow_authority` and only when the quoter requires attested flow —
    /// the current flow key lives there, never on this instance (see
    /// `crate::velocity`). Registered on the quoter entry's quote leg.
    #[account(address = VELOCITY_STATE @ crate::error::MidpointError::InvalidVelocityState)]
    pub velocity_state: UncheckedAccount,
}

/// Declared by `quoter-spec`, which owns every shape on this wire. A local
/// mirror is not a convenience here: the fields are read in declaration
/// order, so a mirror that is missing one reads every field after it from the
/// wrong place and reports nothing wrong.
///
/// The midpoint reads `users`, `taker` and `limit_price`. It ignores `caps`
/// and `reference_price`: it settles against one standing-intent user and
/// holds no orders, so there is no per-user budget to spend and nothing to
/// skip mid-book.
pub use quoter_spec::QuoteArgsV0;

/// Whether this quoter has anything to say to this caller: settleability,
/// self-trade, and (when configured) flow attestation. The mid-staleness /
/// pause gate lives in `MidpointQuoterV0::is_quoting`, applied by the
/// quote/fill walks themselves.
///
/// The attestation branch reads velocity's live `State.hot_flow_authority`, so
/// an unassigned role or a rotated key takes effect for every instance at
/// once. An unassigned role closes the gate.
pub fn caller_gate(
    quoter: &MidpointQuoterV0,
    users: &[UserRefV0],
    taker: Option<&UserRefV0>,
    instructions_sysvar: &anchor_lang_v2::pinocchio::account::AccountView,
    velocity_state: &anchor_lang_v2::pinocchio::account::AccountView,
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
    if quoter.require_attested_flow != 0 {
        let Some(flow_authority) = hot_flow_authority(velocity_state)? else {
            return Ok(false);
        };
        if !tx_co_signed_by(instructions_sysvar, &flow_authority)? {
            return Ok(false);
        }
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
        ctx.accounts.instructions_sysvar.account(),
        ctx.accounts.velocity_state.account(),
    )?;
    ctx.accounts.quoter.write_quote_response(
        args.direction,
        args.size,
        args.limit_price,
        clock.slot,
        open,
    )
}
