use {
    crate::{
        book::ClobBook,
        emit::emit_execute_record,
        error::ClobError,
        state::{ClobMarketV0, Direction, ResponsePointerV0, UserRefV0, UserSetV0},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct ExecuteV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct ExecuteArgsV0 {
    pub direction: Direction,
    pub size: u64,
    /// `User`s velocity has loaded and can settle. Empty = unrestricted
    /// (tests; velocity always passes the loaded set).
    pub users: UserSetV0,
    /// The taker's `User`: their own resting orders are skipped
    /// unconditionally (self-trade prevention).
    pub taker: Option<UserRefV0>,
}

/// Quoter interface: commit a fill; balance changes (merged by user) are
/// streamed into the market's response tail as the book is consumed, located
/// by the returned pointer. Velocity clamps `size` to margin before calling
/// and validates the changes on its side.
pub fn handle_execute_v0(
    ctx: &mut Context<ExecuteV0>,
    args: ExecuteArgsV0,
) -> Result<ResponsePointerV0> {
    let clock = Clock::get()?;
    let market = &mut ctx.accounts.market;
    let market_index = market.market_index;
    let outcome = market.execute(
        args.direction,
        args.size,
        args.users.as_slice(),
        args.taker.as_ref(),
        clock.slot,
        clock.unix_timestamp,
    )?;

    emit_execute_record(
        clock.unix_timestamp,
        clock.slot,
        market_index,
        args.direction.to_u8(),
        &outcome.fills,
        outcome.cancelled_order_id,
    )?;

    Ok(outcome.response)
}
