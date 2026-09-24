//! Writing a prelaunch market's oracle.

use super::*;

#[access_control(
    valid_oracle_for_perp_market(&ctx.accounts.oracle, &ctx.accounts.perp_market)
)]
pub fn handle_update_prelaunch_oracle(ctx: Context<UpdatePrelaunchOracle>) -> Result<()> {
    let clock = Clock::get()?;
    let clock_slot = clock.slot;
    let oracle_map = OracleMap::load_one(
        &ctx.accounts.oracle,
        clock_slot,
        ctx.accounts.state.load()?.slot_clock(),
        None,
    )?;

    let perp_market = &load!(ctx.accounts.perp_market)?;

    validate!(
        perp_market.oracle_source == OracleSource::Prelaunch,
        ErrorCode::InvalidOracle,
        "wrong oracle source"
    )?;

    update_prelaunch_oracle(perp_market, &oracle_map, clock_slot)?;

    Ok(())
}

#[derive(Accounts)]
pub struct UpdatePrelaunchOracle<'info> {
    pub state: AccountLoader<'info, State>,
    pub perp_market: AccountLoader<'info, PerpMarket>,
    #[account(mut)]
    /// CHECK: checked in ix
    pub oracle: UncheckedAccount<'info>,
}
