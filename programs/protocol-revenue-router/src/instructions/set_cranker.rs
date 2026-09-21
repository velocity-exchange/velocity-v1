use {
    crate::{errors::RouterError, instructions::set_admin::AdminUpdate},
    anchor_lang::prelude::*,
};

pub fn set_cranker(ctx: Context<AdminUpdate>, new_cranker: Pubkey) -> Result<()> {
    require_keys_neq!(
        new_cranker,
        Pubkey::default(),
        RouterError::InvalidAuthority
    );
    ctx.accounts.config.cranker = new_cranker;
    ctx.accounts.emit_updated()
}
