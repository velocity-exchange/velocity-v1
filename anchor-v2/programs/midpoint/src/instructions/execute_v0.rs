use {
    crate::{
        emit::emit_pod,
        error::MidpointError,
        events::{MidpointExecuteRecordV0, MIDPOINT_EVENT_VERSION},
        instructions::quote_v0::caller_gate,
        state::{Direction, MidpointQuoterV0, ResponsePointerV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct ExecuteV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    #[account(address = quoter.execute_authority @ MidpointError::InvalidAuthority)]
    pub execute_authority: Signer,
}

/// Declared by `quoter-spec`; see [`crate::instructions::quote_v0`] for why
/// this program does not keep its own.
pub use quoter_spec::ExecuteArgsV0;

/// Quoter interface: commit a fill against the spline. The response carries
/// exactly one balance change (the quoted user) and never cancels anything
/// (standing intent has no orders). Velocity clamps `size` to the quoted
/// user's margin before calling and validates at-or-better on its side.
pub fn handle_execute_v0(
    ctx: &mut Context<ExecuteV0>,
    args: ExecuteArgsV0<'_>,
) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    let open = caller_gate(
        &ctx.accounts.quoter,
        args.users,
        args.taker.as_ref(),
        args.taker_served_window,
    )?;
    // A mid outside the band of velocity's oracle fills nothing, so a
    // compromised hot key cannot settle the maker at an off-market price.
    let open = open
        && ctx
            .accounts
            .quoter
            .mid_within_deviation(args.reference_price);
    let quoter = &mut ctx.accounts.quoter;

    let mut change = None;
    if open {
        let fill = quoter.fill(args.direction, args.size, clock.slot)?;
        if fill.base > 0 {
            // Applying the fill also asserts the consumed rungs are a
            // monotone best-first prefix of the side.
            quoter.apply_fill(args.direction, &fill)?;
            emit_pod!(MidpointExecuteRecordV0 {
                user_authority: quoter.user_authority,
                ts: clock.unix_timestamp,
                slot: clock.slot,
                mid_price: quoter.mid_price,
                base_size: fill.base,
                quote_size: fill.quote,
                configured_market_index: quoter.market_index,
                sub_account_id: quoter.user_sub_account_id,
                direction: match args.direction {
                    Direction::Long => 0,
                    Direction::Short => 1,
                },
                version: MIDPOINT_EVENT_VERSION,
                _pad: [0; 2],
            });
            change = Some((fill.base, fill.quote));
        }
    }

    quoter.write_execute_response(change)
}
