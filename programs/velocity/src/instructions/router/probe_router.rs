//! Test-build-only router probe: quote N registered quoters, split the taker
//! size across their books (priority-tier waterfall), optionally execute
//! each allocation and enforce at-or-better-than-quote on the returned
//! balance changes. Compiled only with `anchor-test` — never in devnet or mainnet
//! builds (an execute without balance-change settlement must not be landable
//! anywhere real). Dies when the real router fill lands.

use std::collections::BTreeMap;

use anchor_lang::prelude::*;

use crate::error::ErrorCode;
use crate::math::router::{split_across_quoters, QuoterBook};
use crate::state::prop_amm::{Direction, ExecuteArgsV0, QuoteArgsV0, QuoterV0};
use crate::state::state::State;
use crate::validate;

#[derive(Accounts)]
pub struct ProbeRouter<'info> {
    pub state: AccountLoader<'info, State>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize)]
pub struct ProbeRouterArgs {
    pub direction: Direction,
    pub size: u64,
    pub users: Option<Vec<Pubkey>>,
    /// The first `quoter_count` remaining accounts are `QuoterV0` entries;
    /// the rest are the union of their registered CPI accounts (plus the
    /// quoter programs).
    pub quoter_count: u8,
    pub execute: bool,
}

pub fn handle_probe_router<'info>(
    ctx: Context<'info, ProbeRouter<'info>>,
    args: ProbeRouterArgs,
) -> Result<()> {
    let state = ctx.accounts.state.load()?;
    let count = args.quoter_count as usize;
    validate!(
        count > 0 && count <= ctx.remaining_accounts.len(),
        ErrorCode::DefaultError,
        "probe quoter_count out of range"
    )?;

    let account_map: BTreeMap<Pubkey, AccountInfo<'info>> = ctx.remaining_accounts[count..]
        .iter()
        .map(|info| (*info.key, info.clone()))
        .collect();

    let quoters: Vec<AccountLoader<QuoterV0>> = ctx.remaining_accounts[..count]
        .iter()
        .map(AccountLoader::try_from)
        .collect::<Result<_>>()?;

    let books: Vec<(u8, Vec<_>)> = quoters
        .iter()
        .map(|loader| {
            let quoter = loader.load()?;
            let levels = quoter.quote(
                QuoteArgsV0 {
                    direction: args.direction,
                    size: args.size,
                    users: args.users.clone(),
                    taker: None,
                },
                &state.signer,
                state.signer_nonce,
                &account_map,
            )?;
            Ok((quoter.priority, levels))
        })
        .collect::<Result<_>>()?;
    let book_refs: Vec<QuoterBook> = books
        .iter()
        .map(|(priority, levels)| QuoterBook {
            priority: *priority,
            levels,
        })
        .collect();
    let allocations = split_across_quoters(args.direction, args.size, &book_refs)?;
    for (i, allocation) in allocations.iter().enumerate() {
        msg!(
            "probe split {}: base {} quote {}",
            i,
            allocation.base,
            allocation.quote
        );
    }

    if args.execute {
        for (i, allocation) in allocations.iter().enumerate() {
            if allocation.base == 0 {
                continue;
            }
            let response = quoters[i].load()?.execute(
                ExecuteArgsV0 {
                    direction: args.direction,
                    size: allocation.base,
                    users: args.users.clone(),
                    taker: None,
                },
                &state.signer,
                state.signer_nonce,
                &account_map,
            )?;
            let (filled_base, filled_quote) = response.balance_changes.iter().try_fold(
                (0u64, 0u64),
                |(base, quote), change| -> Result<(u64, u64)> {
                    Ok((
                        base.checked_add(change.base_size)
                            .ok_or(ErrorCode::MathError)?,
                        quote
                            .checked_add(change.quote_size)
                            .ok_or(ErrorCode::MathError)?,
                    ))
                },
            )?;
            // At-or-better than the quoted levels, per unit (cross-multiply
            // avoids division): a long taker must not pay more, a short
            // taker must not receive less.
            validate!(
                filled_base <= allocation.base,
                ErrorCode::DefaultError,
                "quoter {} overfilled: {} > {}",
                i,
                filled_base,
                allocation.base
            )?;
            if filled_base > 0 {
                let actual = (filled_quote as u128)
                    .checked_mul(allocation.base as u128)
                    .ok_or(ErrorCode::MathError)?;
                let quoted = (allocation.quote as u128)
                    .checked_mul(filled_base as u128)
                    .ok_or(ErrorCode::MathError)?;
                let at_or_better = match args.direction {
                    Direction::Long => actual <= quoted,
                    Direction::Short => actual >= quoted,
                };
                validate!(
                    at_or_better,
                    ErrorCode::DefaultError,
                    "quoter {} filled worse than quoted",
                    i
                )?;
            }
            msg!(
                "probe execute {}: base {} quote {} ({} changes, {} cancelled)",
                i,
                filled_base,
                filled_quote,
                response.balance_changes.len(),
                response.cancelled.len()
            );
        }
    }
    Ok(())
}
