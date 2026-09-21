use {
    crate::{dfx_redemption, errors::RouterError, instructions::set_admin::AdminUpdate},
    anchor_lang::prelude::*,
};

/// The treasury's ATA must not alias the router ATA or the redemption vault.
pub fn validate_treasury(treasury: &Pubkey, config: &Pubkey) -> Result<()> {
    require_keys_neq!(*treasury, Pubkey::default(), RouterError::InvalidAuthority);
    require_keys_neq!(*treasury, *config, RouterError::InvalidTreasury);
    let (redemption_config, _) = Pubkey::find_program_address(&[b"config"], &dfx_redemption::ID);
    require_keys_neq!(*treasury, redemption_config, RouterError::InvalidTreasury);
    Ok(())
}

pub fn set_treasury(ctx: Context<AdminUpdate>, new_treasury: Pubkey) -> Result<()> {
    validate_treasury(&new_treasury, &ctx.accounts.config.key())?;
    ctx.accounts.config.treasury = new_treasury;
    ctx.accounts.emit_updated()
}
