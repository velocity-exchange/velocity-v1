use {
    crate::{errors::RouterError, instructions::set_admin::AdminUpdate},
    anchor_lang::prelude::*,
};

pub fn set_treasury(ctx: Context<AdminUpdate>, new_treasury: Pubkey) -> Result<()> {
    require_keys_neq!(
        new_treasury,
        Pubkey::default(),
        RouterError::InvalidAuthority
    );
    ctx.accounts.config.treasury = new_treasury;
    ctx.accounts.emit_updated()
}
