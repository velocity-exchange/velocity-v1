//! Who resolves the book's own crank work.
//!
//! The book states when a turner must wake: an order past its `max_ts`, an
//! order that reaches its activation slot, a side at its eviction threshold,
//! and the two sides crossing. All four are facts about this account, so the
//! conditions that watch for them live on it. The book keeps every condition
//! current as it places and removes orders. No caller passes a second account
//! to hold a hint, so no hint goes stale because a caller omitted it.
//!
//! The book does not state what to do about any of them. Removing an order
//! releases a maker's margin reservation, pays a reward, and frees a trigger
//! slot the order carried. The book holds none of those. The program that owns
//! the flow registers its own resolver here, and the conditions wake into it.
//! That program can do whatever else it needs on the way through. The book
//! does not constrain it.
//!
//! The place authority signs, because a book's flow already belongs to that
//! key.

/// Declared by `clob-wire`.
pub use clob_wire::{CrankAccountV0, CrankBlockV0, CrankConditionsArgsV0, CrankResolverV0};
use {
    crate::{
        error::ClobError,
        instructions::GatedMarketV0,
        state::{
            CRANK_ACTIVATION, CRANK_BLOCK_OFFSET, CRANK_CAPACITY, CRANK_CROSS, CRANK_EXPIRY,
            CRANK_RESOLVER_CAPACITY, SIDE_COUNTS_BYTES, SIDE_COUNTS_OFFSET, TOP_OF_BOOK_BYTES,
            TOP_OF_BOOK_OFFSET,
        },
    },
    anchor_lang::prelude::*,
    relay_spec::{AccountRefV0, ConditionBlock, ConditionV0, CrankSpecV0},
};

pub fn handle_set_crank_conditions_v0(
    ctx: &mut Context<GatedMarketV0>,
    args: CrankConditionsArgsV0,
) -> Result<CrankBlockV0> {
    require!(
        args.accounts.len() <= CRANK_RESOLVER_CAPACITY,
        ClobError::InvalidConfig
    );

    let refs: Vec<AccountRefV0> = args
        .accounts
        .iter()
        .map(|account| AccountRefV0 {
            address: account.address,
            writable: account.writable as u8,
        })
        .collect();
    let watched = ctx.accounts.market.address().to_bytes();
    let market = &mut ctx.accounts.market;
    let fail = |_| Error::from(ClobError::InvalidConfig);

    // A block written by an older spec is migrated first. The slots below are
    // then in the shape this program addresses them by.
    market.crank.migrate().map_err(fail)?;
    let resolvers = market.crank.write_resolvers(&refs).map_err(fail)?;

    // The conditions come from the book's own state rather than from
    // arguments. The two account watches point at this account. One covers the
    // side counts and one covers the two best prices. Each pair of `u32`s is
    // adjacent, so one region covers it.
    let spec = |resolver: &CrankResolverV0| CrankSpecV0 {
        resolver_program: resolver.program,
        resolver_disc: resolver.disc,
        min_payment: resolver.min_payment,
    };
    let self_watch = |offset: usize, len: usize, resolver: &CrankResolverV0| {
        ConditionV0::on_account_change(
            relay_spec::WatchedRegion::new(watched, offset as u32, len as u32),
            spec(resolver),
            resolvers,
        )
    };
    let conditions = [
        (
            CRANK_EXPIRY,
            args.expiry.program,
            ConditionV0::at_timestamp(market.next_expiry_ts, spec(&args.expiry), resolvers),
        ),
        (
            CRANK_ACTIVATION,
            args.activation.program,
            ConditionV0::at_slot(
                market.next_activation_slot,
                spec(&args.activation),
                resolvers,
            ),
        ),
        (
            CRANK_CAPACITY,
            args.capacity.program,
            self_watch(SIDE_COUNTS_OFFSET, SIDE_COUNTS_BYTES, &args.capacity),
        ),
        (
            CRANK_CROSS,
            args.cross.program,
            self_watch(TOP_OF_BOOK_OFFSET, TOP_OF_BOOK_BYTES, &args.cross),
        ),
    ];

    // A resolver with a zeroed program is a condition the caller does not want.
    // The slot goes inactive instead of waking into nothing.
    for (index, program, condition) in conditions {
        if program == [0u8; 32] {
            market.crank.deactivate_condition(index).map_err(fail)?;
        } else {
            market
                .crank
                .write_condition(index, &condition)
                .map_err(fail)?;
        }
    }

    Ok(CrankBlockV0 {
        block_offset: CRANK_BLOCK_OFFSET as u32,
        top_of_book_offset: TOP_OF_BOOK_OFFSET as u32,
        top_of_book_len: TOP_OF_BOOK_BYTES as u32,
    })
}
