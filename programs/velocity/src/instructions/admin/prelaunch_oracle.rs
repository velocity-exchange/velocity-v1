//! The prelaunch oracle account, which an admin prices by hand.
//!
//! A market that has no external feed yet reads its price from this account.
//! The admin creates it, writes the price and the confidence into it, and
//! deletes it once the market moves to a real feed.

use super::*;

pub fn handle_initialize_prelaunch_oracle(
    ctx: Context<InitializePrelaunchOracle>,
    params: PrelaunchOracleParams,
) -> Result<()> {
    let mut oracle = ctx.accounts.prelaunch_oracle.load_init()?;
    msg!("perp market {}", params.perp_market_index);

    oracle.perp_market_index = params.perp_market_index;
    if let Some(price) = params.price {
        oracle.price = price;
    }
    if let Some(max_price) = params.max_price {
        oracle.max_price = max_price;
    }

    oracle.validate()?;

    Ok(())
}

pub fn handle_update_prelaunch_oracle_params(
    ctx: Context<UpdatePrelaunchOracleParams>,
    params: PrelaunchOracleParams,
) -> Result<()> {
    let mut oracle = ctx.accounts.prelaunch_oracle.load_mut()?;
    let mut perp_market = ctx.accounts.perp_market.load_mut()?;
    msg!("perp market {}", perp_market.market_index);

    let now = Clock::get()?.unix_timestamp;

    if let Some(price) = params.price {
        oracle.price = price;

        msg!("before mark twap ts = {:?} mark twap = {:?} mark twap 5min = {:?} bid twap = {:?} ask twap {:?}", perp_market.market_stats.last_mark_price_twap_ts, perp_market.market_stats.last_mark_price_twap, perp_market.market_stats.last_mark_price_twap_5min, perp_market.market_stats.last_bid_price_twap, perp_market.market_stats.last_ask_price_twap);

        perp_market.market_stats.last_mark_price_twap_ts = now;
        perp_market.market_stats.last_mark_price_twap = price.cast()?;
        perp_market.market_stats.last_mark_price_twap_5min = price.cast()?;
        perp_market.market_stats.last_bid_price_twap = perp_market
            .market_stats
            .last_bid_price_twap
            .min(price.cast()?);
        perp_market.market_stats.last_ask_price_twap = perp_market
            .market_stats
            .last_ask_price_twap
            .max(price.cast()?);

        msg!("after mark twap ts = {:?} mark twap = {:?} mark twap 5min = {:?} bid twap = {:?} ask twap {:?}", perp_market.market_stats.last_mark_price_twap_ts, perp_market.market_stats.last_mark_price_twap, perp_market.market_stats.last_mark_price_twap_5min, perp_market.market_stats.last_bid_price_twap, perp_market.market_stats.last_ask_price_twap);
    } else {
        msg!("mark twap ts, mark twap, mark twap 5min, bid twap, ask twap: unchanged");
    }

    if let Some(max_price) = params.max_price {
        msg!("max price: {:?} -> {:?}", oracle.max_price, max_price);
        oracle.max_price = max_price;
    } else {
        msg!("max price: unchanged")
    }

    oracle.validate()?;

    Ok(())
}

pub fn handle_delete_prelaunch_oracle(
    ctx: Context<DeletePrelaunchOracle>,
    _perp_market_index: u16,
) -> Result<()> {
    let perp_market = ctx.accounts.perp_market.load()?;
    msg!("perp market {}", perp_market.market_index);

    validate!(
        perp_market.oracle != ctx.accounts.prelaunch_oracle.key(),
        ErrorCode::DefaultError,
        "prelaunch oracle currently in use"
    )?;

    Ok(())
}

#[derive(Accounts)]
#[instruction(params: PrelaunchOracleParams,)]
pub struct InitializePrelaunchOracle<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        init,
        seeds = [b"prelaunch_oracle".as_ref(), params.perp_market_index.to_le_bytes().as_ref()],
        space = PrelaunchOracle::SIZE,
        bump,
        payer = admin
    )]
    pub prelaunch_oracle: AccountLoader<'info, PrelaunchOracle>,
    pub state: AccountLoader<'info, State>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(params: PrelaunchOracleParams,)]
pub struct UpdatePrelaunchOracleParams<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"prelaunch_oracle".as_ref(), params.perp_market_index.to_le_bytes().as_ref()],
        bump,
    )]
    pub prelaunch_oracle: AccountLoader<'info, PrelaunchOracle>,
    #[account(
        mut,
        constraint = perp_market.load()?.market_index == params.perp_market_index
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    pub state: AccountLoader<'info, State>,
}

#[derive(Accounts)]
#[instruction(perp_market_index: u16,)]
pub struct DeletePrelaunchOracle<'info> {
    #[account(mut, constraint = check_warm(&admin.key(), &state)?)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [b"prelaunch_oracle".as_ref(), perp_market_index.to_le_bytes().as_ref()],
        bump,
        close = admin
    )]
    pub prelaunch_oracle: AccountLoader<'info, PrelaunchOracle>,
    #[account(
        constraint = perp_market.load()?.market_index == perp_market_index
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    pub state: AccountLoader<'info, State>,
}
