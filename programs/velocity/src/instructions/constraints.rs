use {
    crate::{
        error::ErrorCode,
        msg,
        state::{
            insurance_fund_stake::InsuranceFundStake,
            market_status::MarketStatus,
            perp_market::PerpMarket,
            spot_market::SpotMarket,
            state::{ExchangeStatus, State},
            user::{User, UserStats},
        },
        validate,
    },
    anchor_lang::{
        accounts::{account_loader::AccountLoader, signer::Signer},
        prelude::{AccountInfo, Pubkey, *},
    },
    anchor_spl::token_interface::Mint,
};

pub fn can_sign_for_user(user: &AccountLoader<User>, signer: &Signer) -> anchor_lang::Result<bool> {
    user.load().map(|user| {
        user.authority.eq(signer.key)
            || (user.delegate.eq(signer.key) && !user.delegate.eq(&Pubkey::default()))
    })
}

/// A `User` owned by the protocol itself: the velocity signer PDA's
/// sub-account 0, which no one can sign for. The crank rewards and the
/// cross-match surplus accrue to it and only the hot-role withdraw can take
/// value out.
///
/// Both halves are load-bearing. `initialize_user` takes its authority
/// unchecked, so anyone can pay to create the signer PDA's sub-account 1, 2,
/// … as well. An authority-only test would accept those, and the paths that
/// waive the signature for the protocol `User` would then credit a
/// sub-account the protocol does not track. Every `User` lives at the PDA of
/// `(authority, sub_account_id)`, so the pair names exactly one account.
pub fn is_protocol_user(
    user: &AccountLoader<User>,
    state: &AccountLoader<State>,
) -> anchor_lang::Result<bool> {
    let user = user.load()?;
    Ok(user.sub_account_id == 0 && user.authority.eq(&state.load()?.signer))
}

/// `can_sign_for_user`, relaxed for the dual-mode cranks: the caller signs
/// for the filler as today, **or** the filler is the protocol `User` — the
/// program-keeper mode, where the reward accrues to the protocol and the
/// caller is paid reservoir lamports instead, so no signature is required
/// (relay turners submit executors without one).
pub fn can_crank_for_filler(
    filler: &AccountLoader<User>,
    authority: &AccountInfo,
    state: &AccountLoader<State>,
) -> anchor_lang::Result<bool> {
    if is_protocol_user(filler, state)? {
        return Ok(true);
    }
    let filler = filler.load()?;
    Ok(authority.is_signer
        && (filler.authority.eq(authority.key)
            || (filler.delegate.eq(authority.key) && !filler.delegate.eq(&Pubkey::default()))))
}

pub fn is_stats_for_user(
    user: &AccountLoader<User>,
    user_stats: &AccountLoader<UserStats>,
) -> anchor_lang::Result<bool> {
    let user = user.load()?;
    let user_stats = user_stats.load()?;
    Ok(user_stats.authority.eq(&user.authority))
}

pub fn is_stats_for_if_stake(
    if_stake: &AccountLoader<InsuranceFundStake>,
    user_stats: &AccountLoader<UserStats>,
) -> anchor_lang::Result<bool> {
    let if_stake = if_stake.load()?;
    let user_stats = user_stats.load()?;
    Ok(user_stats.authority.eq(&if_stake.authority))
}

pub fn perp_market_valid(market: &AccountLoader<PerpMarket>) -> anchor_lang::Result<()> {
    if market.load()?.status == MarketStatus::Delisted {
        return Err(ErrorCode::MarketDelisted.into());
    }
    Ok(())
}

pub fn spot_market_valid(market: &AccountLoader<SpotMarket>) -> anchor_lang::Result<()> {
    if market.load()?.status == MarketStatus::Delisted {
        return Err(ErrorCode::MarketDelisted.into());
    }
    Ok(())
}

pub fn valid_oracle_for_spot_market(
    oracle: &AccountInfo,
    market: &AccountLoader<SpotMarket>,
) -> anchor_lang::Result<()> {
    validate!(
        market.load()?.oracle.eq(oracle.key),
        ErrorCode::InvalidOracle,
        "not valid_oracle_for_spot_market"
    )?;
    Ok(())
}

pub fn valid_oracle_for_perp_market(
    oracle: &AccountInfo,
    market: &AccountLoader<PerpMarket>,
) -> anchor_lang::Result<()> {
    validate!(
        market.load()?.oracle.eq(oracle.key),
        ErrorCode::InvalidOracle,
        "not valid_oracle_for_perp_market"
    )?;
    Ok(())
}

pub fn liq_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state
        .get_exchange_status()?
        .contains(ExchangeStatus::LiqPaused)
    {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn funding_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state.funding_paused()? {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn amm_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state.amm_paused()? {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn fill_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state
        .get_exchange_status()?
        .contains(ExchangeStatus::FillPaused)
    {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn deposit_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state
        .get_exchange_status()?
        .contains(ExchangeStatus::DepositPaused)
    {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn withdraw_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state
        .get_exchange_status()?
        .contains(ExchangeStatus::WithdrawPaused)
    {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn solvency_repair_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state.solvency_repair_paused()? {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn settle_pnl_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state
        .get_exchange_status()?
        .contains(ExchangeStatus::SettlePnlPaused)
    {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn exchange_not_paused(state: &AccountLoader<State>) -> anchor_lang::Result<()> {
    let state = state.load()?;
    if state.get_exchange_status()?.is_all() {
        return Err(ErrorCode::ExchangePaused.into());
    }
    Ok(())
}

pub fn get_vault_len(mint: &InterfaceAccount<Mint>) -> anchor_lang::Result<usize> {
    let mint_info = mint.to_account_info();
    let len = if *mint_info.owner == ::anchor_spl::token_2022::Token2022::id() {
        use ::anchor_spl::token_2022::spl_token_2022::{
            extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
            state::{Account, Mint},
        };
        let mint_data = mint_info.try_borrow_data()?;
        let mint_state = StateWithExtensions::<Mint>::unpack(&mint_data)?;
        let mint_extensions = match mint_state.get_extension_types() {
            Ok(extensions) => extensions,
            // If we cant deserialize the mint, try assuming no extensions
            // Init token will fail if this size doesnt work, so worst case init account just fails
            Err(_) => vec![],
        };
        let mut required_extensions =
            ExtensionType::get_required_init_account_extensions(&mint_extensions);
        required_extensions.push(ExtensionType::ImmutableOwner);
        ExtensionType::try_calculate_account_len::<Account>(&required_extensions)?
    } else {
        ::anchor_spl::token::TokenAccount::LEN
    };

    Ok(len)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{create_account_info, create_anchor_account_info, state::state::State},
        anchor_lang::prelude::AccountLoader,
        std::str::FromStr,
    };

    /// `initialize_user` takes its authority unchecked and derives the `User`
    /// PDA from `(authority, sub_account_id)`. Anyone can therefore pay to
    /// create the velocity signer's sub-account 1. It must not read as the
    /// protocol `User`, because the crank paths waive the caller's signature
    /// for that identity and credit it the reward and the cross-match surplus.
    #[test]
    fn only_sub_account_zero_is_the_protocol_user() {
        let signer = Pubkey::from_str("JCNCMFXo5M5qwUPg2Utu1u6YWp3MbygxqBsBeXXJfrw").unwrap();
        let mut state = State {
            signer,
            ..State::default()
        };
        create_anchor_account_info!(state, State, state_loader);
        let state_loader: AccountLoader<State> = AccountLoader::try_from(&state_loader).unwrap();

        let mut protocol = User {
            authority: signer,
            sub_account_id: 0,
            ..User::default()
        };
        create_anchor_account_info!(protocol, User, protocol_info);
        let protocol_loader: AccountLoader<User> = AccountLoader::try_from(&protocol_info).unwrap();
        assert!(is_protocol_user(&protocol_loader, &state_loader).unwrap());

        let mut impostor = User {
            authority: signer,
            sub_account_id: 1,
            ..User::default()
        };
        create_anchor_account_info!(impostor, User, impostor_info);
        let impostor_loader: AccountLoader<User> = AccountLoader::try_from(&impostor_info).unwrap();
        assert!(
            !is_protocol_user(&impostor_loader, &state_loader).unwrap(),
            "a sibling sub-account of the signer PDA is not the protocol user"
        );

        // And an unsigned caller cannot crank for it.
        let authority_key = Pubkey::default();
        let mut lamports = 0;
        let authority_info = AccountInfo::new(
            &authority_key,
            false,
            false,
            &mut lamports,
            &mut [],
            &crate::ID,
            false,
        );
        assert!(
            !can_crank_for_filler(&impostor_loader, &authority_info, &state_loader).unwrap(),
            "an impostor filler still needs a signature"
        );
    }
}
