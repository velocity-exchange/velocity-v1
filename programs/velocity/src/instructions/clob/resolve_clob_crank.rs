//! One resolver for every condition a market's CLOB cranks wake on.
//!
//! Relay names the resolver per condition and hands it the condition that
//! fired, precisely so one resolver can answer for several. Velocity used to
//! spend an instruction per condition instead — three endpoints with the same
//! accounts, the same linkage check and the same staging scaffolding, differing
//! only in which question they asked the book. This is that dispatch, in one
//! place.
//!
//! Each condition keeps its own `min_payment`, because that lives on the
//! condition rather than the resolver: expiry and capacity are held to a
//! removal's payment, a cross to the cheaper of the two crosses its answer can
//! stage. Sharing a resolver costs nothing there.
//!
//! Knowing which condition fired is what makes this free rather than wasteful:
//! a wake on the book's side counts does not go looking for a cross.
//!
//! The fired condition is not authenticated, and does not need to be. It only
//! chooses which work to look for — the staged executor is validated when it
//! lands, and relay independently holds the keeper's balance growth to the
//! fired condition's floor. A caller that lies about it either gets work that
//! is genuinely there or a crank that fails the payment check. What is checked
//! here is that it names one of the two blocks this resolver holds, so a
//! nonsense target cannot be read as a slot.

use {
    super::{
        crank_clob_evict::stage_eviction,
        crank_clob_remove_expired::stage_expired_removal,
        crank_common::{validate_linkage, ResolveClobCrank},
        crank_cross_match::stage_cross,
    },
    crate::{
        error::ErrorCode, instructions::relay_harness::resolve_into,
        state::clob_crank::CLOB_CRANK_CROSS_FALLBACK, validate,
    },
    anchor_lang::prelude::*,
};

/// Which condition relay is asking about.
///
/// Byte-identical to `relay_spec::FiredConditionV0`, which is what the turner
/// appends to a resolver's instruction data. Declared here because that crate
/// carries no borsh derives, and stated in velocity's own types — a `Pubkey`
/// and a `u32` rather than the byte arrays a `Pod` layout needs — so the IDL
/// reads as an argument list instead of a blob. `tests::the_fired_condition_is
/// _what_relay_appends` pins the two encodings together.
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
    /// An order past its expiry, or a side grown to its eviction threshold —
    /// the book's own removals.
    Removal(Removal),
    /// The book crossing itself, whether by a new best, an order reaching its
    /// activation slot, or a PropAMM repricing into it.
    Cross,
}

enum Removal {
    Expired,
    Evictable,
}

impl FiredConditionArgV0 {
    /// Which work this condition is asking about.
    ///
    /// Two accounts host blocks and their slots are numbered independently, so
    /// the account decides the reading before the index does: the book's slots
    /// are `clob-wire`'s, and velocity's one slot is the cross fallback poll.
    fn work(&self, ctx: &Context<ResolveClobCrank>) -> Result<ClobCrankWork> {
        use crate::state::prop_amm::{
            CLOB_CRANK_SLOT_ACTIVATION, CLOB_CRANK_SLOT_CAPACITY, CLOB_CRANK_SLOT_CROSS,
            CLOB_CRANK_SLOT_EXPIRY,
        };
        if self.target == ctx.accounts.clob_market.key() {
            return match self.index {
                CLOB_CRANK_SLOT_EXPIRY => Ok(ClobCrankWork::Removal(Removal::Expired)),
                CLOB_CRANK_SLOT_CAPACITY => Ok(ClobCrankWork::Removal(Removal::Evictable)),
                CLOB_CRANK_SLOT_CROSS | CLOB_CRANK_SLOT_ACTIVATION => Ok(ClobCrankWork::Cross),
                other => {
                    msg!(
                        "book condition slot {} is not a crank velocity serves",
                        other
                    );
                    Err(ErrorCode::DefaultError.into())
                }
            };
        }
        validate!(
            self.target == ctx.accounts.crank_conditions.key(),
            ErrorCode::DefaultError,
            "fired condition names {}, which is neither this market's book nor its conditions",
            self.target
        )?;
        validate!(
            usize::from(self.index) == CLOB_CRANK_CROSS_FALLBACK,
            ErrorCode::DefaultError,
            "conditions slot {} is not a crank velocity serves",
            self.index
        )?;
        Ok(ClobCrankWork::Cross)
    }
}

pub fn handle_resolve_clob_crank(
    ctx: Context<ResolveClobCrank>,
    fired: FiredConditionArgV0,
) -> Result<()> {
    validate_linkage(&ctx)?;
    let work = fired.work(&ctx)?;
    resolve_into(&ctx.accounts.scratch, || match work {
        ClobCrankWork::Removal(Removal::Expired) => stage_expired_removal(&ctx),
        ClobCrankWork::Removal(Removal::Evictable) => stage_eviction(&ctx),
        ClobCrankWork::Cross => stage_cross(&ctx),
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
        // And the same width, so nothing trails or truncates.
        let mut ours = Vec::new();
        decoded.serialize(&mut ours).unwrap();
        assert_eq!(ours, wire.to_bytes());
    }
}
