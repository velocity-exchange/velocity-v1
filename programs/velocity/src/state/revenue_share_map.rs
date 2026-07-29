use {
    crate::{
        error::{ErrorCode, VelocityResult},
        math::safe_unwrap::SafeUnwrap,
        msg,
        state::{revenue_share::RevenueShare, traits::Size, user::User},
        validate,
    },
    anchor_lang::{prelude::AccountLoader, Discriminator},
    arrayref::array_ref,
    solana_program::{account_info::AccountInfo, pubkey::Pubkey},
    std::{cell::RefMut, collections::BTreeMap, iter::Peekable, panic::Location, slice::Iter},
};

#[derive(Default)]
pub struct RevenueShareEntry<'a> {
    pub user: Option<AccountLoader<'a, User>>,
    pub revenue_share: Option<AccountLoader<'a, RevenueShare>>,
}

pub struct RevenueShareMap<'a>(pub BTreeMap<Pubkey, RevenueShareEntry<'a>>);

impl<'a> RevenueShareMap<'a> {
    pub fn empty() -> Self {
        RevenueShareMap(BTreeMap::new())
    }

    pub fn insert_user(
        &mut self,
        authority: Pubkey,
        user_loader: AccountLoader<'a, User>,
    ) -> VelocityResult {
        let entry = self.0.entry(authority).or_default();
        validate!(
            entry.user.is_none(),
            ErrorCode::DefaultError,
            "Duplicate User for authority {:?}",
            authority
        )?;
        entry.user = Some(user_loader);
        Ok(())
    }

    pub fn insert_revenue_share(
        &mut self,
        authority: Pubkey,
        revenue_share_loader: AccountLoader<'a, RevenueShare>,
    ) -> VelocityResult {
        let entry = self.0.entry(authority).or_default();
        validate!(
            entry.revenue_share.is_none(),
            ErrorCode::DefaultError,
            "Duplicate RevenueShare for authority {:?}",
            authority
        )?;
        entry.revenue_share = Some(revenue_share_loader);
        Ok(())
    }

    #[track_caller]
    #[inline(always)]
    pub fn get_user_ref_mut(&self, authority: &Pubkey) -> VelocityResult<RefMut<'_, User>> {
        let loader = match self.0.get(authority).and_then(|e| e.user.as_ref()) {
            Some(loader) => loader,
            None => {
                let caller = Location::caller();
                msg!(
                    "Could not find user for authority {} at {}:{}",
                    authority,
                    caller.file(),
                    caller.line()
                );
                return Err(ErrorCode::UserNotFound);
            }
        };

        match loader.load_mut() {
            Ok(user) => Ok(user),
            Err(e) => {
                let caller = Location::caller();
                msg!("{:?}", e);
                msg!(
                    "Could not load user for authority {} at {}:{}",
                    authority,
                    caller.file(),
                    caller.line()
                );
                Err(ErrorCode::UnableToLoadUserAccount)
            }
        }
    }

    #[track_caller]
    #[inline(always)]
    pub fn get_revenue_share_account_mut(
        &self,
        authority: &Pubkey,
    ) -> VelocityResult<RefMut<'_, RevenueShare>> {
        let loader = match self.0.get(authority).and_then(|e| e.revenue_share.as_ref()) {
            Some(loader) => loader,
            None => {
                let caller = Location::caller();
                msg!(
                    "Could not find revenue share for authority {} at {}:{}",
                    authority,
                    caller.file(),
                    caller.line()
                );
                return Err(ErrorCode::UnableToLoadRevenueShareAccount);
            }
        };

        match loader.load_mut() {
            Ok(revenue_share) => Ok(revenue_share),
            Err(e) => {
                let caller = Location::caller();
                msg!("{:?}", e);
                msg!(
                    "Could not load revenue share for authority {} at {}:{}",
                    authority,
                    caller.file(),
                    caller.line()
                );
                Err(ErrorCode::UnableToLoadRevenueShareAccount)
            }
        }
    }
}

pub fn load_revenue_share_map<'a: 'b, 'b>(
    account_info_iter: &mut Peekable<Iter<'a, AccountInfo<'b>>>,
) -> VelocityResult<RevenueShareMap<'b>> {
    let mut revenue_share_map = RevenueShareMap::empty();

    let user_discriminator: &[u8] = User::DISCRIMINATOR;
    let rev_share_discriminator: &[u8] = RevenueShare::DISCRIMINATOR;

    while let Some(account_info) = account_info_iter.peek() {
        let data = account_info
            .try_borrow_data()
            .or(Err(ErrorCode::DefaultError))?;

        if data.len() < 8 {
            break;
        }

        let account_discriminator = &data[..8];

        if account_discriminator == user_discriminator {
            let user_account_info = account_info_iter.next().safe_unwrap()?;
            let is_writable = user_account_info.is_writable;
            if !is_writable {
                return Err(ErrorCode::UserWrongMutability);
            }

            // Extract authority from User account data (after discriminator)
            let data = user_account_info
                .try_borrow_data()
                .or(Err(ErrorCode::CouldNotLoadUserData))?;
            let expected_data_len = User::SIZE;
            if data.len() < expected_data_len {
                return Err(ErrorCode::CouldNotLoadUserData);
            }
            let authority_slice = array_ref![data, 8, 32];
            let authority = Pubkey::from(*authority_slice);

            let user_account_loader: AccountLoader<User> =
                AccountLoader::try_from(user_account_info)
                    .or(Err(ErrorCode::InvalidUserAccount))?;

            // Builder/referrer revenue-share payouts have a single canonical
            // recipient: sub_account_id 0 of the stored authority (referrer
            // registration already enforces this). The escrow records only the
            // authority, so a permissionless settlement caller could otherwise
            // supply any sibling subaccount of that authority and redirect the
            // accrued rewards. Pin the map to subaccount 0.
            let sub_account_id = user_account_loader
                .load()
                .or(Err(ErrorCode::UnableToLoadUserAccount))?
                .sub_account_id;
            validate!(
                sub_account_id == 0,
                ErrorCode::InvalidRevenueShareRecipient,
                "revenue share recipient for authority {} must be sub_account_id 0, got {}",
                authority,
                sub_account_id
            )?;

            revenue_share_map.insert_user(authority, user_account_loader)?;
            continue;
        }

        if account_discriminator == rev_share_discriminator {
            let revenue_share_account_info = account_info_iter.next().safe_unwrap()?;
            let is_writable = revenue_share_account_info.is_writable;
            if !is_writable {
                return Err(ErrorCode::DefaultError);
            }

            let authority_slice = array_ref![data, 8, 32];
            let authority = Pubkey::from(*authority_slice);

            let revenue_share_account_loader: AccountLoader<RevenueShare> =
                AccountLoader::try_from(revenue_share_account_info)
                    .or(Err(ErrorCode::InvalidRevenueShareAccount))?;

            revenue_share_map.insert_revenue_share(authority, revenue_share_account_loader)?;
            continue;
        }

        break;
    }

    Ok(revenue_share_map)
}
