//! Flow-attestation check. A quoter CPI never sees real signer bits
//! (velocity forwards everything `is_signer: false` by design — see the
//! order-flow doc), so "did the flow authority co-sign this transaction"
//! is answered by introspecting the instructions sysvar instead.

use {
    crate::error::MidpointError,
    anchor_lang::{
        address_eq,
        pinocchio::{account::AccountView, address::Address, sysvars::instructions::Instructions},
        prelude::*,
    },
};

/// Whether `signer` is a signer meta on any top-level instruction of the
/// currently executing transaction. `sysvar` must be the instructions
/// sysvar account — `Instructions::try_from` verifies the address, so a
/// spoofed account fails closed.
pub fn tx_co_signed_by(sysvar: &AccountView, signer: &Address) -> Result<bool> {
    let instructions =
        Instructions::try_from(sysvar).map_err(|_| MidpointError::InvalidInstructionsSysvar)?;
    for index in 0..instructions.num_instructions() {
        let instruction = instructions
            .load_instruction_at(index)
            .map_err(|_| MidpointError::InvalidInstructionsSysvar)?;
        for meta in 0..instruction.num_account_metas() {
            let account = instruction
                .get_instruction_account_at(meta)
                .map_err(|_| MidpointError::InvalidInstructionsSysvar)?;
            if account.is_signer() && address_eq(&account.key, signer) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
