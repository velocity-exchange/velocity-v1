use std::collections::BTreeMap;

use anchor_lang::{prelude::*, solana_program::program::{get_return_data, invoke}};
use solana_program::instruction::Instruction;

#[derive(Eq, PartialEq, Debug, Default, Copy, Clone)]
#[repr(C)]
pub enum AmmType {
    Vamm,
    Clob,
    #[default]
    Custom,
}

#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug)]
#[repr(C)]
pub struct PropAmmV0 {
    pub amm_type: AmmType,
    pub is_active: bool,
    pub is_approved: bool,
    pub market: u16,
    pub user: Pubkey,
    pub program_id: Pubkey,
    pub quote_v0_discriminator: [u8; 8],
    pub execute_v0_discriminator: [u8; 8],
    pub quote_accounts_count: u8,
    pub quote_accounts: [AmmAccountMeta; 32],
    pub execute_accounts_count: u8,
    pub execute_accounts: [AmmAccountMeta; 32],
}

#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug)]
#[repr(C)]
pub struct AmmAccountMeta {
    pub pubkey: Pubkey,
    pub is_writable: bool,
}

#[derive(Eq, PartialEq, Debug)]
#[repr(C)]
pub struct PriceLevel {
    pub price: u64,
    pub size: u64,
}

impl PropAmmV0 {
    pub fn quote<'info>(
        &self,
        size: u64,
        account_map: BTreeMap<Pubkey, AccountInfo<'info>>,
    ) -> Result<Vec<PriceLevel>> {
        let accounts = self.quote_accounts[..(self.quote_accounts_count as usize)]
            .iter()
            .flat_map(|acc| account_map.get(&acc.pubkey))
            .cloned()
            .collect::<Vec<_>>();
        let desc = self.quote_v0_discriminator;
        let data = [desc.as_slice(), size.to_le_bytes().as_slice()].concat();
        invoke(
            &Instruction {
                program_id: self.program_id,
                data,
                accounts: self.quote_accounts[..(self.quote_accounts_count as usize)]
                    .iter()
                    .map(|acc| AccountMeta {
                        pubkey: acc.pubkey,
                        is_signer: account_map.get(&acc.pubkey).map_or(false, |v| v.is_signer),
                        is_writable: acc.is_writable,
                    })
                    .collect(),
            },
            &accounts,
        )?;
        let return_value = get_return_data();
        Ok(res.)
    }

    pub fn execute(&self) -> Result<()> {
        Ok(())
    }
}
