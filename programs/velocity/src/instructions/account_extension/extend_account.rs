//! Permissionless grow of a zero-copy account to the size the deployed
//! program compiles in for its type.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer, Transfer};
use anchor_lang::Discriminator;

use crate::error::ErrorCode;
use crate::state::insurance_fund_stake::InsuranceFundStake;
use crate::state::oracle::PrelaunchOracle;
use crate::state::perp_market::PerpMarket;
use crate::state::pyth_lazer_oracle::PythLazerOracle;
use crate::state::revenue_share::RevenueShare;
use crate::state::spot_market::SpotMarket;
use crate::state::state::State;
use crate::state::user::{ReferrerName, User, UserStats};
use crate::validate;
use crate::vlp::hedge::state::{Constituent, LPPool};

#[derive(Accounts)]
pub struct ExtendAccount<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: must be velocity-owned; the handler resolves its type (and target
    /// size) from the account discriminator
    #[account(mut, owner = crate::ID)]
    pub account: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Grow `account` to `8 + size_of::<T>()` for its discriminator's type `T`.
///
/// Grow-only: an account already at (or beyond) the target size is a no-op so
/// repeated cranking and races are harmless. The payer covers the rent-exempt
/// shortfall for the new size; `resize` zero-fills the added tail, which every
/// zero-copy struct treats as padding until a later field claims it.
pub fn handle_extend_account(ctx: Context<ExtendAccount>) -> Result<()> {
    let account = &ctx.accounts.account;

    let target_len = {
        let data = account.try_borrow_data()?;
        validate!(
            data.len() >= 8,
            ErrorCode::InvalidAccountExtension,
            "account too small for a discriminator"
        )?;
        extension_target_len(&data[..8]).ok_or_else(|| {
            msg!("discriminator does not match a supported zero-copy account");
            ErrorCode::InvalidAccountExtension
        })?
    };

    let current_len = account.data_len();
    if current_len >= target_len {
        return Ok(());
    }

    let required_lamports = Rent::get()?.minimum_balance(target_len);
    let shortfall = required_lamports.saturating_sub(account.lamports());
    if shortfall > 0 {
        transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to: account.to_account_info(),
                },
            ),
            shortfall,
        )?;
    }

    account.resize(target_len).map_err(Into::<Error>::into)?;

    Ok(())
}

/// Full on-chain size (`8 + size_of::<T>()`) for the zero-copy account type
/// matching `discriminator`, or `None` for anything else. Borsh accounts are
/// excluded on purpose: their decode is field-by-field, so appending fields is
/// a deserialization change, not a trailing-bytes change, and gets its own
/// migration per account type.
pub fn extension_target_len(discriminator: &[u8]) -> Option<usize> {
    macro_rules! match_target {
        ($($t:ty),+ $(,)?) => {
            $(
                if discriminator == <$t as Discriminator>::DISCRIMINATOR {
                    return Some(8 + std::mem::size_of::<$t>());
                }
            )+
        };
    }
    match_target!(
        User,
        UserStats,
        ReferrerName,
        PerpMarket,
        SpotMarket,
        State,
        InsuranceFundStake,
        PrelaunchOracle,
        PythLazerOracle,
        RevenueShare,
        LPPool,
        Constituent,
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::traits::Size;

    #[test]
    fn target_len_covers_zero_copy_accounts() {
        assert_eq!(
            extension_target_len(User::DISCRIMINATOR),
            Some(8 + std::mem::size_of::<User>())
        );
        assert_eq!(
            extension_target_len(PerpMarket::DISCRIMINATOR),
            Some(8 + std::mem::size_of::<PerpMarket>())
        );
        assert_eq!(
            extension_target_len(SpotMarket::DISCRIMINATOR),
            Some(8 + std::mem::size_of::<SpotMarket>())
        );
        assert_eq!(
            extension_target_len(State::DISCRIMINATOR),
            Some(8 + std::mem::size_of::<State>())
        );

        // target must agree with the declared account SIZE used at init
        assert_eq!(extension_target_len(User::DISCRIMINATOR), Some(User::SIZE));
        assert_eq!(
            extension_target_len(PerpMarket::DISCRIMINATOR),
            Some(PerpMarket::SIZE)
        );
        assert_eq!(
            extension_target_len(SpotMarket::DISCRIMINATOR),
            Some(SpotMarket::SIZE)
        );
    }

    #[test]
    fn target_len_rejects_unknown_and_borsh_accounts() {
        use crate::state::signed_msg_user::SignedMsgUserOrders;

        assert_eq!(extension_target_len(&[0u8; 8]), None);
        assert_eq!(extension_target_len(&[]), None);
        // borsh (non-zero-copy) accounts are not extendable via this path
        assert_eq!(
            extension_target_len(SignedMsgUserOrders::DISCRIMINATOR),
            None
        );
    }
}
