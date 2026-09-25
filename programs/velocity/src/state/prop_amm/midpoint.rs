//! What velocity reads of a midpoint instance, and nothing more.
//!
//! The midpoint is a `Custom` quoter, so fills treat it like any other one.
//! Approval reads three fields of the instance account. A wrong
//! `execute_authority` quotes and then fails every execute. A `size_step` off
//! the market's step makes the instance fill a size the router did not
//! allocate. The instance must also serve the entry's market.
//!
//! allow-verbose: the offsets are the midpoint program's account layout,
//! which lives in another workspace, so they are stated here with their
//! derivation.
//! The account is an 8-byte discriminator and then `MidpointQuoterV0`, whose
//! header is packed in declaration order: `authority`, `hot_authority`,
//! `execute_authority` (32 bytes each), `user_authority`, then eight `u64`s
//! from `mid_price` to `base_precision`, then `user_sub_account_id` and
//! `market_index` (`u16` each). `size_step` is the sixth `u64`.

use {
    crate::{error::ErrorCode, validate},
    anchor_lang::prelude::*,
};

/// The midpoint quoter program.
pub const MIDPOINT_PROGRAM_ID: Pubkey = pubkey!("eb3Kwmht4evPGGonNHCQs1h7ng63ZUwZ9TyV1qPo23D");

/// `sha256("account:MidpointQuoterV0")[..8]`.
const MIDPOINT_QUOTER_DISCRIMINATOR: [u8; 8] = [20, 248, 215, 56, 82, 220, 184, 164];

const EXECUTE_AUTHORITY_OFFSET: usize = 8 + 2 * 32;
const SIZE_STEP_OFFSET: usize = 8 + 4 * 32 + 5 * 8;
const MARKET_INDEX_OFFSET: usize = 8 + 4 * 32 + 8 * 8 + 2;
const HEADER_END: usize = MARKET_INDEX_OFFSET + 2;

/// The fields of a midpoint instance that approval checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidpointInstance {
    pub execute_authority: Pubkey,
    pub size_step: u64,
    pub market_index: u16,
}

impl MidpointInstance {
    /// Read an instance account the midpoint program owns.
    pub fn read(info: &AccountInfo) -> Result<Self> {
        validate!(
            info.owner == &MIDPOINT_PROGRAM_ID,
            ErrorCode::InvalidQuoterConfig,
            "midpoint instance {} is not owned by the midpoint program",
            info.key
        )?;

        let data = info
            .try_borrow_data()
            .map_err(|_| error!(ErrorCode::InvalidQuoterConfig))?;
        Self::parse(&data)
    }

    fn parse(data: &[u8]) -> Result<Self> {
        validate!(
            data.len() >= HEADER_END && data[..8] == MIDPOINT_QUOTER_DISCRIMINATOR,
            ErrorCode::InvalidQuoterConfig,
            "account does not hold a midpoint instance"
        )?;

        let word = |offset: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&data[offset..offset + 8]);
            u64::from_le_bytes(bytes)
        };
        let mut execute_authority = [0u8; 32];
        execute_authority
            .copy_from_slice(&data[EXECUTE_AUTHORITY_OFFSET..EXECUTE_AUTHORITY_OFFSET + 32]);
        Ok(Self {
            execute_authority: Pubkey::new_from_array(execute_authority),
            size_step: word(SIZE_STEP_OFFSET),
            market_index: u16::from_le_bytes([
                data[MARKET_INDEX_OFFSET],
                data[MARKET_INDEX_OFFSET + 1],
            ]),
        })
    }

    /// Hold the instance to the market it is approved on. It must accept the
    /// market's slab as its execute signer, serve the market, and step its
    /// sizes on the market's step grid.
    pub fn validate_for(
        &self,
        slab: &Pubkey,
        market_index: u16,
        order_step_size: u64,
    ) -> Result<()> {
        validate!(
            self.execute_authority == *slab,
            ErrorCode::InvalidQuoterConfig,
            "midpoint execute authority {} is not the market's quoter slab {}",
            self.execute_authority,
            slab
        )?;
        validate!(
            self.market_index == market_index,
            ErrorCode::InvalidQuoterConfig,
            "midpoint serves market {}, entry is for market {}",
            self.market_index,
            market_index
        )?;
        validate!(
            order_step_size > 0
                && self.size_step > 0
                && self.size_step.is_multiple_of(order_step_size),
            ErrorCode::InvalidQuoterConfig,
            "midpoint size step {} is not a multiple of the market step {}",
            self.size_step,
            order_step_size
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance_bytes(execute_authority: Pubkey, size_step: u64, market_index: u16) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_END + 64];
        data[..8].copy_from_slice(&MIDPOINT_QUOTER_DISCRIMINATOR);
        data[EXECUTE_AUTHORITY_OFFSET..EXECUTE_AUTHORITY_OFFSET + 32]
            .copy_from_slice(execute_authority.as_ref());
        data[SIZE_STEP_OFFSET..SIZE_STEP_OFFSET + 8].copy_from_slice(&size_step.to_le_bytes());
        data[MARKET_INDEX_OFFSET..MARKET_INDEX_OFFSET + 2]
            .copy_from_slice(&market_index.to_le_bytes());
        data
    }

    #[test]
    fn the_discriminator_is_the_anchor_account_hash() {
        let hash = solana_program::hash::hash(b"account:MidpointQuoterV0");
        assert_eq!(hash.to_bytes()[..8], MIDPOINT_QUOTER_DISCRIMINATOR);
    }

    #[test]
    fn an_instance_bound_to_the_slab_on_the_market_step_is_approvable() {
        let slab = Pubkey::new_unique();
        let instance = MidpointInstance::parse(&instance_bytes(slab, 2_000, 3)).unwrap();
        assert_eq!(instance.size_step, 2_000);
        assert!(instance.validate_for(&slab, 3, 1_000).is_ok());
    }

    #[test]
    fn an_instance_that_cannot_execute_or_steps_off_the_grid_is_refused() {
        let slab = Pubkey::new_unique();
        let instance = |authority, step, market| {
            MidpointInstance::parse(&instance_bytes(authority, step, market)).unwrap()
        };

        // Another key as execute authority quotes and then fails every execute.
        assert!(instance(Pubkey::new_unique(), 1_000, 3)
            .validate_for(&slab, 3, 1_000)
            .is_err());
        assert!(instance(slab, 1_500, 3)
            .validate_for(&slab, 3, 1_000)
            .is_err());
        assert!(instance(slab, 1_000, 4)
            .validate_for(&slab, 3, 1_000)
            .is_err());
    }

    #[test]
    fn an_account_of_another_layout_is_refused() {
        let mut data = instance_bytes(Pubkey::new_unique(), 1_000, 3);
        data[0] ^= 1;
        assert!(MidpointInstance::parse(&data).is_err());
        assert!(MidpointInstance::parse(&data[..HEADER_END - 1]).is_err());
    }
}
