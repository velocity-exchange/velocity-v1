//! One resolver for every condition a market's CLOB cranks wake on.
//!
//! Relay names the resolver per condition and passes the condition that fired,
//! so one resolver can answer for several conditions. The alternative is one
//! endpoint per condition, each with the same accounts, the same linkage check
//! and the same staging scaffolding. This file holds that dispatch in one
//! place.
//!
//! Each condition keeps its own `min_payment`, because the payment floor lives
//! on the condition rather than on the resolver. Expiry and capacity are held
//! to a removal's payment. A cross is held to the cheaper of the two crosses
//! its answer can stage. A shared resolver costs nothing there.
//!
//! The fired condition tells the resolver where to look, so a wake on the
//! book's side counts does not search for a cross.
//!
//! The fired condition is not authenticated, and does not need to be. It only
//! chooses which work to look for. The staged executor is validated when it
//! lands, and relay holds the keeper's balance growth to the fired condition's
//! floor on its own. A caller that lies about the condition either gets work
//! that is there or a crank that fails the payment check. The one check here is
//! that the condition names one of the two blocks this resolver holds, so a
//! nonsense target cannot be read as a slot.

use {
    super::{
        crank_clob_evict::stage_eviction,
        crank_clob_remove_expired::stage_expired_removal,
        crank_common::{validate_linkage, ResolveClobCrank},
        crank_cross_match::stage_cross,
        refill_crank_reservoir::stage_refill,
    },
    crate::{
        error::ErrorCode,
        instructions::relay_harness::resolve_into,
        state::clob_crank::{CLOB_CRANK_CROSS_FALLBACK, CLOB_CRANK_REFILL},
        validate,
    },
    anchor_lang::prelude::*,
};

/// Which condition relay is asking about.
///
/// Byte-identical to `relay_spec::FiredConditionV0`, which is what the turner
/// appends to a resolver's instruction data. It is declared here because that
/// crate carries no borsh derives. It uses velocity's own types, a `Pubkey` and
/// a `u32`, rather than the byte arrays a `Pod` layout needs, so the IDL reads
/// as an argument list instead of a blob. The test
/// `the_fired_condition_is_what_relay_appends` pins the two encodings
/// together.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FiredConditionArgV0 {
    /// The account holding the condition block.
    pub target: Pubkey,
    /// Byte offset of that block within the account.
    pub block_offset: u32,
    /// Slot of the condition within the block.
    pub index: u8,
}

const _: () = assert!(
    32 + 4 + 1 == relay_spec::FIRED_CONDITION_LEN,
    "the fired condition must encode as relay writes it"
);

/// What a market's CLOB cranks can be woken for.
enum ClobCrankWork {
    /// An order past its expiry, or a side that grew to its eviction
    /// threshold. Both are the book's own removals.
    Removal(Removal),
    /// The book crossing itself, whether by a new best, an order reaching its
    /// activation slot, or a PropAMM repricing into it.
    Cross,
    /// The reservoir that pays this market's cranks has fallen to its
    /// watermark and is due a refill from the protocol treasury.
    Refill,
}

enum Removal {
    Expired,
    Evictable,
}

impl FiredConditionArgV0 {
    /// Which work this condition is asking about.
    ///
    /// Two accounts host condition blocks, and their slots are numbered
    /// independently. So the account decides the reading before the index does.
    /// The book's slots are `clob-wire`'s. The conditions account holds
    /// velocity's own slots, the cross fallback poll and the reservoir
    /// refill.
    fn work(&self, ctx: &Context<ResolveClobCrank>) -> Result<ClobCrankWork> {
        use crate::state::prop_amm::{
            CRANK_SLOT_ACTIVATION, CRANK_SLOT_CAPACITY, CRANK_SLOT_CROSS, CRANK_SLOT_EXPIRY,
        };

        if self.target == ctx.accounts.clob_market.key() {
            return match self.index {
                CRANK_SLOT_EXPIRY => Ok(ClobCrankWork::Removal(Removal::Expired)),
                CRANK_SLOT_CAPACITY => Ok(ClobCrankWork::Removal(Removal::Evictable)),
                CRANK_SLOT_CROSS | CRANK_SLOT_ACTIVATION => Ok(ClobCrankWork::Cross),
                other => {
                    msg!(
                        "book condition slot {} is not a crank velocity serves",
                        other
                    );

                    Err(ErrorCode::UnrecognizedCrankCondition.into())
                }
            };
        }

        validate!(
            self.target == ctx.accounts.crank_conditions.key(),
            ErrorCode::UnrecognizedCrankCondition,
            "fired condition names {}, which is neither this market's book nor its conditions",
            self.target
        )?;

        match usize::from(self.index) {
            CLOB_CRANK_CROSS_FALLBACK => Ok(ClobCrankWork::Cross),
            CLOB_CRANK_REFILL => Ok(ClobCrankWork::Refill),
            other => {
                msg!("conditions slot {} is not a crank velocity serves", other);
                Err(ErrorCode::UnrecognizedCrankCondition.into())
            }
        }
    }
}

pub fn handle_resolve_clob_crank(
    ctx: Context<ResolveClobCrank>,
    fired: FiredConditionArgV0,
) -> Result<()> {
    // The book's own account rides writable, because `quote_l3_v0` streams the
    // answer into its response tail. It belongs to the CLOB program rather than
    // to velocity, so the check passes over it and the staging region stays
    // just the scratch account.
    crate::instructions::constraints::require_view_accounts(
        &ctx.accounts.to_account_infos(),
        &[ctx.accounts.scratch.key()],
    )?;

    validate_linkage(&ctx)?;
    let work = fired.work(&ctx)?;
    resolve_into(&ctx.accounts.scratch, || match work {
        ClobCrankWork::Removal(Removal::Expired) => stage_expired_removal(&ctx),
        ClobCrankWork::Removal(Removal::Evictable) => stage_eviction(&ctx),
        ClobCrankWork::Cross => stage_cross(&ctx),
        ClobCrankWork::Refill => stage_refill(&ctx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argument decodes exactly the bytes a turner appends, so a resolver
    /// reads the condition relay meant rather than a shifted view of it.
    #[test]
    fn the_fired_condition_is_what_relay_appends() {
        let target = [7u8; 32];
        let wire = relay_spec::FiredConditionV0::new(target, 184, 3);
        let decoded = FiredConditionArgV0::deserialize(&mut wire.to_bytes().as_slice())
            .expect("decodes as relay wrote it");
        assert_eq!(decoded.target, Pubkey::new_from_array(target));
        assert_eq!(decoded.block_offset, 184);
        assert_eq!(decoded.index, 3);
        // The width matches too, so nothing trails and nothing truncates.
        let mut ours = Vec::new();
        decoded.serialize(&mut ours).unwrap();
        assert_eq!(ours, wire.to_bytes());
    }
}
