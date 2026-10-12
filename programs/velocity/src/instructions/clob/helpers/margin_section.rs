//! The margin-map section a crossing fill needs for the users it settles.
//!
//! A fill checks the margin of every user it moves, and that check reads every
//! market each user holds a position in, with the market's oracle. A resolver
//! holds only the accounts its condition registered, so it cannot see those
//! markets until it reads the users. It therefore asks relay for accounts in
//! rounds: first the users, then the markets their positions name, and then it
//! stages the fill with every oracle. Each round re-derives what it needs from
//! the accounts it was given, so a book that moves between rounds costs one
//! more round rather than a wrong answer.

use {
    crate::{
        error::ErrorCode,
        msg,
        state::{pdas, perp_market::PerpMarket, spot_market::SpotMarket, user::User},
    },
    anchor_lang::{prelude::*, Discriminator},
    std::collections::BTreeSet,
};

/// What a resolver knows about the crossed market without reading any account.
pub struct CrossedMarket {
    pub perp_market_index: u16,
    pub oracle: Pubkey,
    pub quote_spot_market_index: u16,
}

/// The accounts the fill's margin checks read, in the order `load_maps`
/// consumes them. The crossed perp market is not among `perp_markets`,
/// because every executor names it on its own.
pub struct MarginSection {
    pub oracles: Vec<Pubkey>,
    pub spot_markets: Vec<Pubkey>,
    pub quote_spot_market: Pubkey,
    pub perp_markets: Vec<Pubkey>,
}

pub enum MarginAccounts {
    /// Accounts this round was not given. Relay appends them and runs the
    /// resolver again.
    Missing(Vec<Pubkey>),
    Ready(MarginSection),
}

/// The margin section for `users`, or the accounts still needed to build it.
///
/// `available` is every account this round was given. The search is by key,
/// so it does not matter where in the list relay appended them.
pub fn margin_accounts(
    available: &[AccountInfo],
    users: &[Pubkey],
    crossed: &CrossedMarket,
    sol_spot_market_index: u16,
) -> Result<MarginAccounts> {
    let missing_users = missing(available, users.iter().copied());
    if !missing_users.is_empty() {
        return Ok(MarginAccounts::Missing(missing_users));
    }

    let mut held_spot = BTreeSet::from([crossed.quote_spot_market_index]);
    let mut perp_indexes = BTreeSet::new();
    for user in users {
        let info = find(available, user)?;
        let data = info.try_borrow_data()?;
        let user = read::<User>(&data, ErrorCode::CouldNotLoadUserData)?;
        held_spot.extend(
            user.spot_positions
                .iter()
                .filter(|position| !position.is_available())
                .map(|position| position.market_index),
        );
        perp_indexes.extend(
            user.perp_positions
                .iter()
                .filter(|position| !position.is_available())
                .map(|position| position.market_index),
        );
    }

    // The SOL spot market prices the keeper payment off its TWAP, so it rides
    // even when no user holds SOL. Its oracle rides only when one does.
    let mut spot_indexes = held_spot.clone();
    if sol_spot_market_index != 0 {
        spot_indexes.insert(sol_spot_market_index);
    }

    perp_indexes.remove(&crossed.perp_market_index);
    let spot_markets: Vec<Pubkey> = spot_indexes.iter().map(|&i| pdas::spot_market(i)).collect();
    let perp_markets: Vec<Pubkey> = perp_indexes.iter().map(|&i| pdas::perp_market(i)).collect();

    let missing_markets = missing(available, spot_markets.iter().chain(&perp_markets).copied());
    if !missing_markets.is_empty() {
        return Ok(MarginAccounts::Missing(missing_markets));
    }

    let mut oracles = vec![crossed.oracle];
    for &index in &held_spot {
        let data = find(available, &pdas::spot_market(index))?.try_borrow_data()?;
        push_oracle(
            &mut oracles,
            read::<SpotMarket>(&data, ErrorCode::CouldNotLoadSpotMarketData)?.oracle,
        );
    }

    for market in &perp_markets {
        let data = find(available, market)?.try_borrow_data()?;
        push_oracle(
            &mut oracles,
            read::<PerpMarket>(&data, ErrorCode::CouldNotLoadMarketData)?.oracle,
        );
    }

    Ok(MarginAccounts::Ready(MarginSection {
        oracles,
        spot_markets,
        quote_spot_market: pdas::spot_market(crossed.quote_spot_market_index),
        perp_markets,
    }))
}

/// A market with no oracle account stores the default key, and an oracle two
/// markets share is named once.
fn push_oracle(oracles: &mut Vec<Pubkey>, oracle: Pubkey) {
    if oracle != Pubkey::default() && !oracles.contains(&oracle) {
        oracles.push(oracle);
    }
}

fn missing(available: &[AccountInfo], wanted: impl Iterator<Item = Pubkey>) -> Vec<Pubkey> {
    let mut missing: Vec<Pubkey> = Vec::new();
    for key in wanted {
        if !available.iter().any(|info| info.key == &key) && !missing.contains(&key) {
            missing.push(key);
        }
    }

    missing
}

fn find<'a, 'info>(
    available: &'a [AccountInfo<'info>],
    key: &Pubkey,
) -> Result<&'a AccountInfo<'info>> {
    available
        .iter()
        .find(|info| info.key == key)
        .ok_or_else(|| error!(ErrorCode::CouldNotLoadUserData))
}

/// Borrow a velocity zero-copy account by reference. The body sits 8 bytes
/// into the data, which is 8-aligned onchain. Copying it out would put a
/// `User` on a 4 KiB stack.
fn read<T: bytemuck::Pod + Discriminator>(data: &[u8], error: ErrorCode) -> Result<&T> {
    let body = data
        .get(8..8 + std::mem::size_of::<T>())
        .filter(|_| data.starts_with(T::DISCRIMINATOR))
        .ok_or_else(|| error!(error))?;
    bytemuck::try_from_bytes(body).map_err(|_| {
        msg!("account body is misaligned for a zero-copy read");
        error!(error)
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            create_anchor_account_info,
            state::user::{PerpPosition, SpotPosition},
        },
    };

    const QUOTE: u16 = 0;
    const SOL: u16 = 1;
    const CROSSED: u16 = 1;
    const OTHER_PERP: u16 = 2;

    fn crossed(oracle: Pubkey) -> CrossedMarket {
        CrossedMarket {
            perp_market_index: CROSSED,
            oracle,
            quote_spot_market_index: QUOTE,
        }
    }

    /// A maker with SOL collateral and a position in a second perp market.
    fn maker() -> User {
        let mut user = User::default();
        user.spot_positions[0] = SpotPosition {
            market_index: QUOTE,
            scaled_balance: 1,
            ..SpotPosition::default()
        };
        user.spot_positions[1] = SpotPosition {
            market_index: SOL,
            scaled_balance: 1,
            ..SpotPosition::default()
        };
        user.perp_positions[0] = PerpPosition {
            market_index: CROSSED,
            base_asset_amount: 1,
            ..PerpPosition::default()
        };
        user.perp_positions[1] = PerpPosition {
            market_index: OTHER_PERP,
            base_asset_amount: -1,
            ..PerpPosition::default()
        };
        user
    }

    fn missing_of(answer: MarginAccounts) -> Vec<Pubkey> {
        match answer {
            MarginAccounts::Missing(accounts) => accounts,
            MarginAccounts::Ready(_) => panic!("expected missing accounts"),
        }
    }

    #[test]
    fn users_come_first() {
        let users = [Pubkey::new_unique(), Pubkey::new_unique()];
        let answer = margin_accounts(&[], &users, &crossed(Pubkey::new_unique()), SOL).unwrap();
        assert_eq!(missing_of(answer), users.to_vec());
    }

    /// Every market the maker holds, plus the quote and SOL markets the fill
    /// prices against. The crossed market is named by the executor, so it is
    /// never requested.
    #[test]
    fn then_the_markets_their_positions_name() {
        let user_key = Pubkey::new_unique();
        let mut user = maker();
        create_anchor_account_info!(user, &user_key, User, user_info);

        let answer = margin_accounts(
            &[user_info],
            &[user_key],
            &crossed(Pubkey::new_unique()),
            SOL,
        )
        .unwrap();
        assert_eq!(
            missing_of(answer),
            vec![
                pdas::spot_market(QUOTE),
                pdas::spot_market(SOL),
                pdas::perp_market(OTHER_PERP),
            ]
        );
    }

    /// The SOL market prices the keeper payment off its TWAP, so it rides for
    /// a user who holds no SOL, without its oracle.
    #[test]
    fn the_sol_market_rides_without_its_oracle_when_nobody_holds_sol() {
        let crossed_oracle = Pubkey::new_unique();
        let user_key = Pubkey::new_unique();
        let mut user = User::default();
        create_anchor_account_info!(user, &user_key, User, user_info);

        let mut quote = SpotMarket {
            market_index: QUOTE,
            ..SpotMarket::default()
        };
        let mut sol = SpotMarket {
            market_index: SOL,
            oracle: Pubkey::new_unique(),
            ..SpotMarket::default()
        };
        let (quote_key, sol_key) = (pdas::spot_market(QUOTE), pdas::spot_market(SOL));
        create_anchor_account_info!(quote, &quote_key, SpotMarket, quote_info);
        create_anchor_account_info!(sol, &sol_key, SpotMarket, sol_info);

        let available = [user_info, quote_info, sol_info];
        let section = match margin_accounts(&available, &[user_key], &crossed(crossed_oracle), SOL)
            .unwrap()
        {
            MarginAccounts::Ready(section) => section,
            MarginAccounts::Missing(accounts) => panic!("still missing {:?}", accounts),
        };
        assert_eq!(section.oracles, vec![crossed_oracle]);
        assert_eq!(section.spot_markets, vec![quote_key, sol_key]);
    }

    /// The section names each market's oracle once. A market with no oracle
    /// account adds nothing.
    #[test]
    fn then_every_oracle_once() {
        let (crossed_oracle, sol_oracle) = (Pubkey::new_unique(), Pubkey::new_unique());
        let user_key = Pubkey::new_unique();
        let mut user = maker();
        create_anchor_account_info!(user, &user_key, User, user_info);

        let mut quote = SpotMarket {
            market_index: QUOTE,
            ..SpotMarket::default()
        };
        let mut sol = SpotMarket {
            market_index: SOL,
            oracle: sol_oracle,
            ..SpotMarket::default()
        };
        let mut other = PerpMarket {
            market_index: OTHER_PERP,
            oracle: sol_oracle,
            ..PerpMarket::default()
        };
        let (quote_key, sol_key, other_key) = (
            pdas::spot_market(QUOTE),
            pdas::spot_market(SOL),
            pdas::perp_market(OTHER_PERP),
        );
        create_anchor_account_info!(quote, &quote_key, SpotMarket, quote_info);
        create_anchor_account_info!(sol, &sol_key, SpotMarket, sol_info);
        create_anchor_account_info!(other, &other_key, PerpMarket, other_info);

        let available = [other_info, user_info, sol_info, quote_info];
        let section = match margin_accounts(&available, &[user_key], &crossed(crossed_oracle), SOL)
            .unwrap()
        {
            MarginAccounts::Ready(section) => section,
            MarginAccounts::Missing(accounts) => panic!("still missing {:?}", accounts),
        };
        assert_eq!(section.oracles, vec![crossed_oracle, sol_oracle]);
        assert_eq!(section.spot_markets, vec![quote_key, sol_key]);
        assert_eq!(section.quote_spot_market, quote_key);
        assert_eq!(section.perp_markets, vec![other_key]);
    }
}
