use anchor_lang_v2::prelude::*;

use crate::error::ClobError;
use crate::events::{ExecuteRecord, FillSlim};
use crate::state::{
    CancelledRemainderV0, ClobBook, ClobMarketV0, Direction, ExecuteResponseV0, ResponsePointerV0,
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
    /// `User`s velocity has loaded and can settle. `None` = unrestricted
    /// (tests; velocity always passes the loaded set).
    pub users: Option<Vec<Address>>,
    /// The taker's `User`: their own resting orders are skipped
    /// unconditionally (self-trade prevention).
    pub taker: Option<Address>,
}

/// Quoter interface: commit a fill; balance changes (merged by user) go to
/// the market's response tail, located by the returned pointer. Velocity
/// clamps `size` to margin before calling and validates the changes on its
/// side.
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
        args.users.as_deref(),
        args.taker.as_ref(),
        clock.slot,
        clock.unix_timestamp,
    )?;

    emit!(ExecuteRecord {
        ts: clock.unix_timestamp,
        slot: clock.slot,
        market_index,
        direction: args.direction.to_u8(),
        fills: outcome
            .fills
            .iter()
            .map(|f| FillSlim {
                order_id: f.order_id,
                base_size: f.base_size,
            })
            .collect(),
        cancelled_order_ids: outcome.cancelled.iter().map(|c| c.order_id).collect(),
    });

    let mut data = Vec::with_capacity(2048);
    anchor_lang_v2::wincode::config::serialize_into(
        &mut data,
        &ExecuteResponseV0 {
            balance_changes: outcome.balance_changes,
            cancelled: outcome
                .cancelled
                .iter()
                .map(|c| CancelledRemainderV0 {
                    user: c.user,
                    order_id: c.order_id,
                    base_asset_amount: c.base_asset_amount,
                })
                .collect(),
        },
        anchor_lang_v2::BORSH_CONFIG,
    )
    .map_err(|_| ClobError::ResponseTooLarge)?;
    market.write_response(&data)
}
