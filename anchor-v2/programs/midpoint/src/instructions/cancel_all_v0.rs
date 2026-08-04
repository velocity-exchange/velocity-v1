use {
    crate::{
        error::MidpointError,
        state::{CancelAllOutcomeV0, CancelSidesV0, Direction, MidpointQuoterV0},
    },
    anchor_lang_v2::prelude::*,
};

#[derive(Accounts)]
pub struct CancelAllV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    /// Either of the maker's keys — matched in the handler, since anchor's
    /// `address =` locks to one. The hot key because withdrawing quotes is part
    /// of the quoting loop and must not need the cold key; the config key
    /// because a maker's panic button must work when the hot key is exactly
    /// what they no longer trust.
    pub authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelAllArgsV0 {
    pub sides: CancelSidesV0,
    /// Also zero the mid, which stops *every* side quoting regardless of what
    /// the ladders hold (see `MidpointQuoterV0::is_quoting`). Cheap belt to the
    /// braces when withdrawing both sides.
    ///
    /// Not a durable kill: the hot key can stamp a new mid straight after. The
    /// durable one is `update_quoter_v0 { is_paused: true }`, which only the
    /// config key can set and only the config key can undo.
    pub clear_mid: bool,
}

/// Withdraw a maker's standing intent on one side (or both) in one instruction.
///
/// The spline has no orders to cancel, so this is the equivalent operation:
/// zero the live rungs so the side quotes nothing until the maker reshapes it.
/// `set_levels_v0` with an empty side already did this, but it pays to
/// deserialize two `Option<Vec<_>>` args and then re-scans both ladders on the
/// way out; this writes only the rungs that were live and re-checks only the
/// sides it touched.
///
/// Emits nothing, for the same reason mid and level writes don't: a maker's
/// shape writes are their own business until a fill makes them the exchange's.
pub fn handle_cancel_all_v0(
    ctx: &mut Context<CancelAllV0>,
    args: CancelAllArgsV0,
) -> Result<CancelAllOutcomeV0> {
    let signer = *ctx.accounts.authority.address();
    let quoter = &mut ctx.accounts.quoter;
    require!(
        signer == quoter.hot_authority || signer == quoter.authority,
        MidpointError::InvalidAuthority
    );

    let mut outcome = CancelAllOutcomeV0::default();
    for direction in args.sides.directions().iter().copied() {
        let rungs = quoter.clear_side(direction);
        match direction {
            Direction::Long => outcome.ask_rungs = rungs,
            Direction::Short => outcome.bid_rungs = rungs,
        }
        quoter.validate_cleared_side(direction, rungs)?;
    }

    if args.clear_mid {
        // Sequence 0: the guard is for racing *price* writers, and a
        // withdrawal must never be the write that loses a race.
        let slot = Clock::get()?.slot;
        quoter.set_mid(0, 0, slot)?;
        outcome.mid_cleared = true;
    }

    Ok(outcome)
}
