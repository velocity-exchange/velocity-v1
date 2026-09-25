use {
    super::user::SpotPosition,
    crate::{
        error::{ErrorCode, VelocityResult},
        math::safe_unwrap::SafeUnwrap,
        msg,
        state::{
            perp_market::PerpMarket,
            traits::{MarketIndexOffset, Size},
            user::PerpPositions,
        },
    },
    anchor_lang::{accounts::account_loader::AccountLoader, prelude::AccountInfo, Discriminator},
    arrayref::array_ref,
    std::{
        cell::{Ref, RefMut},
        collections::{BTreeMap, BTreeSet},
        iter::Peekable,
        panic::Location,
        slice::Iter,
    },
};

pub struct PerpMarketMap<'a>(pub BTreeMap<u16, AccountLoader<'a, PerpMarket>>);

impl<'a> PerpMarketMap<'a> {
    #[track_caller]
    #[inline(always)]
    pub fn get_ref(&self, market_index: &u16) -> VelocityResult<Ref<'_, PerpMarket>> {
        let loader = match self.0.get(market_index) {
            Some(loader) => loader,
            None => {
                let caller = Location::caller();
                msg!(
                    "Could not find perp market {} at {}:{}",
                    market_index,
                    caller.file(),
                    caller.line()
                );
                return Err(ErrorCode::PerpMarketNotFound);
            }
        };

        match loader.load() {
            Ok(perp_market) => Ok(perp_market),
            Err(e) => {
                let caller = Location::caller();
                msg!("{:?}", e);
                msg!(
                    "Could not load perp market {} at {}:{}",
                    market_index,
                    caller.file(),
                    caller.line()
                );
                Err(ErrorCode::UnableToLoadPerpMarketAccount)
            }
        }
    }

    #[track_caller]
    #[inline(always)]
    pub fn get_ref_mut(&self, market_index: &u16) -> VelocityResult<RefMut<'_, PerpMarket>> {
        let loader = match self.0.get(market_index) {
            Some(loader) => loader,
            None => {
                let caller = Location::caller();
                msg!(
                    "Could not find perp market {} at {}:{}",
                    market_index,
                    caller.file(),
                    caller.line()
                );
                return Err(ErrorCode::PerpMarketNotFound);
            }
        };

        match loader.load_mut() {
            Ok(perp_market) => Ok(perp_market),
            Err(e) => {
                let caller = Location::caller();
                msg!("{:?}", e);
                msg!(
                    "Could not load perp market {} at {}:{}",
                    market_index,
                    caller.file(),
                    caller.line()
                );
                Err(ErrorCode::UnableToLoadPerpMarketAccount)
            }
        }
    }

    pub fn load<'b, 'c>(
        writable_markets: &'b MarketSet,
        account_info_iter: &'c mut Peekable<Iter<'a, AccountInfo<'a>>>,
    ) -> VelocityResult<PerpMarketMap<'a>> {
        let mut perp_market_map: PerpMarketMap = PerpMarketMap(BTreeMap::new());

        let market_discriminator: &[u8] = PerpMarket::DISCRIMINATOR;
        while let Some(account_info) = account_info_iter.peek() {
            let data = account_info
                .try_borrow_data()
                .or(Err(ErrorCode::CouldNotLoadMarketData))?;

            let expected_data_len = PerpMarket::SIZE;
            if data.len() < expected_data_len {
                break;
            }

            let account_discriminator = &data[..8];
            if account_discriminator != market_discriminator {
                break;
            }

            // market index `MARKET_INDEX_OFFSET` bytes from front of account (incl 8-byte Anchor disc)
            let market_index =
                u16::from_le_bytes(*array_ref![data, PerpMarket::MARKET_INDEX_OFFSET, 2]);

            crate::validate!(
                !(perp_market_map.0.contains_key(&market_index)),
                ErrorCode::InvalidMarketAccount,
                "Can not include same market index twice {}",
                market_index
            )?;

            let account_info = account_info_iter.next().safe_unwrap()?;

            let is_writable = account_info.is_writable;
            crate::validate!(
                !(writable_markets.contains(&market_index) && !is_writable),
                ErrorCode::MarketWrongMutability,
                "Market {} is not writable",
                market_index
            )?;

            let account_loader: AccountLoader<PerpMarket> =
                AccountLoader::try_from(account_info).or(Err(ErrorCode::InvalidMarketAccount))?;

            perp_market_map.0.insert(market_index, account_loader);
        }

        Ok(perp_market_map)
    }
}

impl<'a> PerpMarketMap<'a> {
    /// A map that holds one market, built from a named account rather than
    /// the positional maps section. It serves an instruction whose accounts
    /// struct names the perp market.
    pub fn load_one<'c: 'a>(
        account_info: &'c AccountInfo<'a>,
        must_be_writable: bool,
    ) -> VelocityResult<PerpMarketMap<'a>> {
        let mut perp_market_map: PerpMarketMap = PerpMarketMap(BTreeMap::new());

        let data = account_info
            .try_borrow_data()
            .or(Err(ErrorCode::CouldNotLoadMarketData))?;

        let expected_data_len = PerpMarket::SIZE;
        if data.len() < expected_data_len {
            return Err(ErrorCode::CouldNotLoadMarketData);
        }

        let market_discriminator: &[u8] = PerpMarket::DISCRIMINATOR;
        let account_discriminator = &data[..8];
        if account_discriminator != market_discriminator {
            return Err(ErrorCode::CouldNotLoadMarketData);
        }

        // market index `MARKET_INDEX_OFFSET` bytes from front of account
        // (offset includes the 8-byte Anchor discriminator).
        let market_index =
            u16::from_le_bytes(*array_ref![data, PerpMarket::MARKET_INDEX_OFFSET, 2]);

        let is_writable = account_info.is_writable;
        let account_loader: AccountLoader<PerpMarket> =
            AccountLoader::try_from(account_info).or(Err(ErrorCode::InvalidMarketAccount))?;

        crate::validate!(
            !(must_be_writable && !is_writable),
            ErrorCode::MarketWrongMutability,
            "Market {} is not writable",
            market_index
        )?;

        perp_market_map.0.insert(market_index, account_loader);

        Ok(perp_market_map)
    }

    pub fn empty() -> Self {
        PerpMarketMap(BTreeMap::new())
    }

    #[cfg(test)]
    pub fn load_multiple<'c: 'a>(
        account_infos: Vec<&'c AccountInfo<'a>>,
        must_be_writable: bool,
    ) -> VelocityResult<PerpMarketMap<'a>> {
        let mut perp_market_map: PerpMarketMap = PerpMarketMap(BTreeMap::new());

        for account_info in account_infos {
            let data = account_info
                .try_borrow_data()
                .or(Err(ErrorCode::CouldNotLoadMarketData))?;

            let expected_data_len = PerpMarket::SIZE;
            if data.len() < expected_data_len {
                return Err(ErrorCode::CouldNotLoadMarketData);
            }

            let market_discriminator: &[u8] = PerpMarket::DISCRIMINATOR;
            let account_discriminator = &data[..8];
            if account_discriminator != market_discriminator {
                return Err(ErrorCode::CouldNotLoadMarketData);
            }

            // market index `MARKET_INDEX_OFFSET` bytes from front of account (incl 8-byte Anchor disc)
            let market_index =
                u16::from_le_bytes(*array_ref![data, PerpMarket::MARKET_INDEX_OFFSET, 2]);

            let is_writable = account_info.is_writable;
            let account_loader: AccountLoader<PerpMarket> =
                AccountLoader::try_from(account_info).or(Err(ErrorCode::InvalidMarketAccount))?;

            crate::validate!(
                !(must_be_writable && !is_writable),
                ErrorCode::MarketWrongMutability,
                "Market {} is not writable",
                market_index
            )?;

            perp_market_map.0.insert(market_index, account_loader);
        }

        Ok(perp_market_map)
    }
}

pub(crate) type MarketSet = BTreeSet<u16>;

pub fn get_writable_perp_market_set(market_index: u16) -> MarketSet {
    let mut writable_markets = MarketSet::new();
    writable_markets.insert(market_index);
    writable_markets
}

pub fn get_writable_perp_market_set_from_vec(market_indexes: &[u16]) -> MarketSet {
    let mut writable_markets = MarketSet::new();
    for market_index in market_indexes.iter() {
        writable_markets.insert(*market_index);
    }
    writable_markets
}

pub fn get_market_set_from_list(market_indexes: Vec<u16>) -> MarketSet {
    let mut writable_markets = MarketSet::new();
    for market_index in market_indexes.iter() {
        writable_markets.insert(*market_index);
    }
    writable_markets
}

pub fn get_market_set_for_user_positions(user_positions: &PerpPositions) -> MarketSet {
    let mut writable_markets = MarketSet::new();
    for position in user_positions.iter() {
        if !position.is_available() {
            writable_markets.insert(position.market_index);
        }
    }
    writable_markets
}

pub fn get_market_set_for_spot_positions(spot_positions: &[SpotPosition]) -> MarketSet {
    let mut writable_markets = MarketSet::new();
    for position in spot_positions.iter() {
        if !position.is_available() {
            writable_markets.insert(position.market_index);
        }
    }
    writable_markets
}
