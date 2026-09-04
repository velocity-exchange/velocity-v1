//! Grow a zero-copy account to the size the deployed program compiles in for
//! its type (auth: `AccountExtension` hot key, or warm/cold admin).

use {
    crate::{
        auth::check_hot,
        error::ErrorCode,
        state::{
            clob_crank::ClobCrankConditionsV0,
            insurance_fund_stake::InsuranceFundStake,
            oracle::PrelaunchOracle,
            perp_market::PerpMarket,
            prop_amm::QuoterV0,
            pyth_lazer_oracle::PythLazerOracle,
            quoter_cross::QuoterCrossConditionsV0,
            revenue_share::RevenueShare,
            spot_market::SpotMarket,
            state::{HotRole, State},
            user::{ReferrerName, User, UserStats},
            user_conditions::UserConditionsV0,
        },
        validate,
        vlp::hedge::state::{Constituent, LPPool},
    },
    anchor_lang::{
        prelude::*,
        system_program::{transfer, Transfer},
        Discriminator,
    },
};

#[derive(Accounts)]
pub struct ExtendAccount<'info> {
    pub state: AccountLoader<'info, State>,
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(constraint = check_hot(&authority.key(), &state, HotRole::AccountExtension)?)]
    pub authority: Signer<'info>,
    /// CHECK: must be velocity-owned; the handler resolves its type (and target
    /// size) from the account discriminator
    #[account(mut, owner = crate::ID)]
    pub account: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Grow `account` to `8 + size_of::<T>()` for its discriminator's type `T`.
///
/// Auth: the `AccountExtension` hot key (or warm/cold admin fallback while the
/// role is unset). Extension is harmless to account contents, but growing
/// accounts inflates fetch bandwidth and any future per-byte transaction
/// costs, so when it happens is the protocol's call, not the public's.
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
        // Relay plumbing. The condition blocks grow whenever relay's
        // `CONDITION_LEN` changes, which is why they belong here rather
        // than in a bespoke migration. A quoter slab is deliberately absent:
        // its size is capacity, not layout, and growing one means growing
        // the account and raising the header's `capacity`, not matching a
        // struct's `SIZE`.
        QuoterV0,
        ClobCrankConditionsV0,
        QuoterCrossConditionsV0,
        UserConditionsV0,
    );
    None
}

#[cfg(test)]
mod tests {
    use {super::*, crate::state::traits::Size};

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
