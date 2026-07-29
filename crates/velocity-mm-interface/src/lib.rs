use {
    serde::{Deserialize, Serialize},
    solana_account::{Account, ReadableAccount},
    solana_clock::Clock,
    solana_program_error::ProgramError,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        hash::BuildHasher,
        io,
        ops::Deref,
        sync::{
            Arc,
            atomic::{AtomicI64, AtomicU64, Ordering},
        },
    },
    thiserror::Error,
};

pub mod serde_utils;
mod swap;
pub use swap::{
    AccountsType, CandidateSwap, RemainingAccountsInfo, RemainingAccountsSlice, Side, Swap,
};
use zeropod::ZeroPod;

#[derive(Debug)]
pub struct QuoteParamsV0 {
    pub amount: u64,
    pub perp_market: u16,
}

#[derive(ZeroPod)]
#[zeropod(compact)]
pub struct AmmQuoteV0 {
    pub reference_price: u64,
    pub levels: zeropod::Vec<PriceLevel, 64>,
}

#[derive(ZeroPod)]
pub struct PriceLevel {
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Error)]
#[error("Could not find address: {0}")]
pub struct AccountNotFoundError(Pubkey);

pub trait AccountProvider {
    fn get(&self, pubkey: &Pubkey) -> Option<impl ReadableAccount + use<'_, Self>>;

    fn try_get(&self, pubkey: &Pubkey) -> Result<impl ReadableAccount, AccountNotFoundError> {
        self.get(pubkey).ok_or(AccountNotFoundError(*pubkey))
    }
}

impl<'a, T: AccountProvider> AccountProvider for &'a T {
    fn get(&self, pubkey: &Pubkey) -> Option<impl ReadableAccount + use<'_, 'a, T>> {
        T::get(self, pubkey)
    }
}

impl<V, S: BuildHasher> AccountProvider for HashMap<Pubkey, V, S>
where
    V: Deref,
    V::Target: ReadableAccount,
{
    fn get(&self, pubkey: &Pubkey) -> Option<impl ReadableAccount + use<'_, V, S>> {
        HashMap::get(self, pubkey).map(Deref::deref)
    }
}

#[derive(Debug, Error)]
pub enum AmmError {
    #[error(transparent)]
    AccountNotFound(#[from] AccountNotFoundError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Program(#[from] ProgramError),
    #[error("{0}")]
    Custom(String),
}

impl From<&str> for AmmError {
    fn from(value: &str) -> Self {
        Self::Custom(value.to_string())
    }
}

impl From<String> for AmmError {
    fn from(value: String) -> Self {
        Self::Custom(value)
    }
}

pub trait Amm: Clone {
    /// Deserializes on-chain account data and optional params into an AMM instance
    fn from_keyed_account(
        keyed_account: &KeyedAccount,
        amm_context: &AmmContext,
    ) -> Result<Self, AmmError>
    where
        Self: Sized;

    /// A human readable label of the underlying DEX
    fn label(&self) -> AmmLabel;

    /// The on-chain program that owns this AMM's accounts and executes its swaps
    fn program_id(&self) -> Pubkey;

    /// The pool state or market state address
    fn key(&self) -> Pubkey;

    /// The perp markets that can be traded
    fn get_markets(&self) -> Vec<u16>;

    /// The accounts necessary to produce a quote
    fn get_accounts_to_quote(&self) -> Vec<Pubkey>;

    // The version of the AMM. Changes to quoting may require quote_v1, etc.
    fn version(&self) -> u16 {
        0
    }

    /// Picks necessary accounts to update it's internal state
    /// Heavy deserialization and precomputation caching should be done in this function
    /// Updates internal quote and returns an offset into the PropAMM account that is the `AmmQuoteV0`
    fn quote_v0(
        &mut self,
        quote_params: QuoteParamsV0,
        account_provider: impl AccountProvider,
    ) -> Result<u32, AmmError>;

    fn execute(
        &mut self,
        quote_params: QuoteParamsV0,
        account_provider: impl AccountProvider,
    ) -> Result<(), AmmError>;

    /// Indicates if get_accounts_to_quote might return a non constant vec
    fn has_dynamic_accounts(&self) -> bool {
        false
    }

    fn get_accounts_len(&self) -> usize {
        32 // Default to a near whole legacy transaction to penalize no implementation
    }

    /// Provides a shortcut to establish if the AMM can be used for trading
    /// If the market is active at all
    fn is_active(&self) -> bool {
        true
    }
}

pub type AmmLabel = &'static str;

pub trait AmmProgramIdToLabel {
    const PROGRAM_ID_TO_LABELS: &[(Pubkey, AmmLabel)];
}

pub trait SingleProgramAmm {
    const PROGRAM_ID: Pubkey;
    const LABEL: AmmLabel;
}

impl<T: SingleProgramAmm> AmmProgramIdToLabel for T {
    const PROGRAM_ID_TO_LABELS: &[(Pubkey, AmmLabel)] = &[(Self::PROGRAM_ID, Self::LABEL)];
}

#[macro_export]
macro_rules! single_program_amm {
    ($amm_struct:ty, $program_id:expr, $label:expr) => {
        impl SingleProgramAmm for $amm_struct {
            const PROGRAM_ID: Pubkey = $program_id;
            const LABEL: AmmLabel = $label;
        }
    };
}

#[derive(Clone, Deserialize, Serialize)]
pub struct KeyedAccount {
    pub key: Pubkey,
    pub account: Account,
    pub params: Option<serde_json::Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Market {
    #[serde(with = "serde_utils::field_as_string")]
    pub pubkey: Pubkey,
    #[serde(with = "serde_utils::field_as_string")]
    pub owner: Pubkey,
    /// Additional data an Amm requires, Amm dependent and decoded in the Amm implementation
    pub params: Option<serde_json::Value>,
}

impl From<KeyedAccount> for Market {
    fn from(
        KeyedAccount {
            key,
            account,
            params,
        }: KeyedAccount,
    ) -> Self {
        Market {
            pubkey: key,
            owner: account.owner,
            params,
        }
    }
}

#[derive(Default)]
pub struct AmmContext {
    pub clock_ref: ClockRef,
}

#[derive(Default)]
pub struct ClockData {
    pub slot: AtomicU64,
    /// The timestamp of the first `Slot` in this `Epoch`.
    pub epoch_start_timestamp: AtomicI64,
    /// The current `Epoch`.
    pub epoch: AtomicU64,
    pub leader_schedule_epoch: AtomicU64,
    pub unix_timestamp: AtomicI64,
}

#[derive(Default, Clone)]
pub struct ClockRef(Arc<ClockData>);

impl Deref for ClockRef {
    type Target = ClockData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ClockRef {
    pub fn update(&self, clock: Clock) {
        self.epoch.store(clock.epoch, Ordering::Relaxed);
        self.slot.store(clock.slot, Ordering::Relaxed);
        self.unix_timestamp
            .store(clock.unix_timestamp, Ordering::Relaxed);
        self.epoch_start_timestamp
            .store(clock.epoch_start_timestamp, Ordering::Relaxed);
        self.leader_schedule_epoch
            .store(clock.leader_schedule_epoch, Ordering::Relaxed);
    }
}

impl From<Clock> for ClockRef {
    fn from(clock: Clock) -> Self {
        ClockRef(Arc::new(ClockData {
            epoch: AtomicU64::new(clock.epoch),
            epoch_start_timestamp: AtomicI64::new(clock.epoch_start_timestamp),
            leader_schedule_epoch: AtomicU64::new(clock.leader_schedule_epoch),
            slot: AtomicU64::new(clock.slot),
            unix_timestamp: AtomicI64::new(clock.unix_timestamp),
        }))
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_pubkey::pubkey};

    #[test]
    fn test_market_deserialization() {
        let json = r#"
        {
            "lamports": 1000,
            "owner": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "pubkey": "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
            "executable": false,
            "rentEpoch": 0
        }
        "#;
        let market: Market = serde_json::from_str(json).unwrap();
        assert_eq!(
            market.owner,
            pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
        );
        assert_eq!(
            market.pubkey,
            pubkey!("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263")
        );
    }
}
