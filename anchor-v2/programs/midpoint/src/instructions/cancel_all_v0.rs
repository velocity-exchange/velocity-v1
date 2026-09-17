use {
    crate::{
        error::MidpointError,
        state::{CancelAllOutcomeV0, CancelSidesExt, CancelSidesV0, Direction, MidpointQuoterV0},
    },
    anchor_lang::prelude::*,
};

#[derive(Accounts)]
pub struct CancelAllV0 {
    #[account(mut)]
    pub quoter: Account<MidpointQuoterV0>,
    /// Either of the maker's keys. The handler matches them, because anchor's
    /// `address =` locks to one key. The hot key works because withdrawing
    /// quotes is part of the quoting loop and must not need the cold key. The
    /// config key works because a withdrawal must still succeed when the maker
    /// no longer trusts the hot key.
    pub authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
#[cfg_attr(feature = "idl-build", derive(anchor_lang::IdlType))]
pub struct CancelAllArgsV0 {
    pub sides: CancelSidesV0,
    /// Also zero the mid, which stops every side from quoting whatever the
    /// ladders hold. See `MidpointQuoterV0::is_quoting`.
    ///
    /// The stop is not durable. The hot key can stamp a new mid at once. The
    /// durable stop is `update_quoter_v0 { is_paused: true }`. Only the config
    /// key can set it, and only the config key can clear it.
    pub clear_mid: bool,
}

/// Withdraw a maker's standing intent on one side, or on both sides, in one
/// instruction.
///
/// The spline holds no orders to cancel, so the equivalent operation zeroes the
/// live rungs. The side then quotes nothing until the maker writes a new shape.
/// `set_levels_v0` with an empty side does the same thing, but it deserializes
/// two `Option<Vec<_>>` arguments and rescans both ladders on the way out. This
/// instruction writes only the rungs that were live and rechecks only the sides
/// it touched.
///
/// It emits nothing, for the reason mid and level writes emit nothing. A
/// maker's shape write is the maker's own record. A fill is the exchange's.
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
        // The withdrawal carries no sequence and consumes none. The monotonic
        // guard exists for racing price writers, and a withdrawal must never
        // lose that race.
        quoter.clear_mid()?;
        outcome.mid_cleared = true;
    }

    Ok(outcome)
}
