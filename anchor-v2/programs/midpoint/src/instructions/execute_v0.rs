use {
    crate::{
        error::MidpointError,
        events::MidpointExecuteRecord,
        instructions::quote_v0::caller_gate,
        state::{
            Direction, ExecuteResponseV0, MidpointQuoterV0, ResponsePointerV0, UserBalanceChange,
            UserRefV0,
        },
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct ExecuteV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    #[account(address = quoter.execute_authority @ MidpointError::InvalidAuthority)]
    pub execute_authority: Signer,
    /// CHECK: the instructions sysvar; see `QuoteV0`.
    pub instructions_sysvar: UncheckedAccount,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ExecuteArgsV0 {
    pub direction: Direction,
    pub size: u64,
    pub users: Option<Vec<UserRefV0>>,
    pub taker: Option<UserRefV0>,
}

/// Quoter interface: commit a fill against the spline. The response carries
/// exactly one balance change (the quoted user) and never cancels anything
/// (standing intent has no orders). Velocity clamps `size` to the quoted
/// user's margin before calling and validates at-or-better on its side.
pub fn handle_execute_v0(
    ctx: &mut Context<ExecuteV0>,
    args: ExecuteArgsV0,
) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    let open = caller_gate(
        &ctx.accounts.quoter,
        args.users.as_deref(),
        args.taker.as_ref(),
        ctx.accounts.instructions_sysvar.account(),
    )?;
    let quoter = &mut ctx.accounts.quoter;

    let mut balance_changes = Vec::new();
    if open {
        let fill = quoter.fill(args.direction, args.size, clock.slot)?;
        if fill.base > 0 {
            quoter.apply_fill(args.direction, &fill);
            emit!(MidpointExecuteRecord {
                authority: quoter.authority,
                ts: clock.unix_timestamp,
                slot: clock.slot,
                mid_price: quoter.mid_price,
                base_size: fill.base,
                quote_size: fill.quote,
                market_index: quoter.market_index,
                sub_account_id: quoter.user_sub_account_id,
                direction: match args.direction {
                    Direction::Long => 0,
                    Direction::Short => 1,
                },
                _pad: [0; 3],
            });
            balance_changes.push(UserBalanceChange {
                user: quoter.user_ref(),
                base_size: fill.base,
                quote_size: fill.quote,
                completed_order_ids: Vec::new(),
            });
        }
    }

    let mut data = Vec::with_capacity(256);
    anchor_lang_v2::wincode::config::serialize_into(
        &mut data,
        &ExecuteResponseV0 {
            balance_changes,
            cancelled: Vec::new(),
        },
        anchor_lang_v2::BORSH_CONFIG,
    )
    .map_err(|_| MidpointError::ResponseTooLarge)?;
    quoter.write_response(&data)
}
