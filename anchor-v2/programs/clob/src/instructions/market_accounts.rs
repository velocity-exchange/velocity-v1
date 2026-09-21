//! The three account shapes every CLOB instruction takes.
//!
//! Fourteen instructions read one of these three shapes. Declaring each one
//! per instruction made the authority constraint a sentence repeated eight
//! times, and a constraint that is repeated is a constraint that can be
//! dropped from one copy without anyone noticing.

use {
    crate::{error::ClobError, state::ClobMarketV0},
    anchor_lang::prelude::*,
};

/// The book, writable, behind its `place_authority`. Velocity signs as that
/// authority, so these are the calls only the registering program may make.
#[derive(Accounts)]
pub struct GatedMarketV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
    #[account(address = market.place_authority @ ClobError::InvalidAuthority)]
    pub place_authority: Signer,
}

/// The book, read only. Anyone may simulate these answers.
#[derive(Accounts)]
pub struct MarketViewV0 {
    pub market: ClobMarketV0,
}

/// The book, writable for the response tail alone. The quoter interface
/// streams its answer into the account, because return data carries only a
/// pointer. The book itself is not changed.
#[derive(Accounts)]
pub struct ResponseMarketV0 {
    #[account(mut)]
    pub market: ClobMarketV0,
}
