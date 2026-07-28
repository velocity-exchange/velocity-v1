//! Test-build-only probe: run a registered quoter's `quote_v0` (and
//! optionally `execute_v0`) CPI legs end to end, so the wire protocol and CU
//! cost are measurable before the router fill exists. Compiled only with
//! `anchor-test` — never in devnet or mainnet builds (an execute without
//! balance-change settlement must not be landable anywhere real).

use std::collections::BTreeMap;

use anchor_lang::prelude::*;

use crate::state::prop_amm::{Direction, ExecuteArgsV0, QuoteArgsV0, QuoterV0};
use crate::state::state::State;

#[derive(Accounts)]
pub struct ProbeQuoter<'info> {
    pub state: AccountLoader<'info, State>,
    pub quoter: AccountLoader<'info, QuoterV0>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct ProbeQuoterArgs {
    pub direction: Direction,
    pub size: u64,
    pub users: Option<Vec<Pubkey>>,
    pub execute: bool,
}

pub fn handle_probe_quoter<'info>(
    ctx: Context<'info, ProbeQuoter<'info>>,
    args: ProbeQuoterArgs,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let quoter = ctx.accounts.quoter.load()?;

    let mut account_map: BTreeMap<Pubkey, AccountInfo<'info>> = BTreeMap::new();
    for info in ctx.remaining_accounts {
        account_map.insert(*info.key, info.clone());
    }

    let levels = quoter.quote(
        QuoteArgsV0 {
            direction: args.direction,
            size: args.size,
            users: args.users.clone(),
        },
        &state.signer,
        state.signer_nonce,
        &account_map,
    )?;
    msg!("probe quote: {} levels", levels.len());

    if args.execute {
        let response = quoter.execute(
            ExecuteArgsV0 {
                direction: args.direction,
                size: args.size,
                users: args.users,
            },
            &state.signer,
            state.signer_nonce,
            &account_map,
        )?;
        msg!(
            "probe execute: {} balance changes, {} cancelled",
            response.balance_changes.len(),
            response.cancelled.len()
        );
    }
    Ok(())
}
