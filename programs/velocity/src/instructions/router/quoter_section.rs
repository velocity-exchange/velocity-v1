//! The quoter section of a router-fill transaction, assembled once.
//!
//! Every router fill ends with the same account tail: the `QuoterV0` registry
//! entries the caller wants consulted, plus the union of their registered CPI
//! accounts (each quoter's program, its response account, the quoter CPI
//! signer). Quoting them is the same work whoever sent the transaction — a
//! keeper cranking someone else's order, or a taker routing their own — so it
//! lives here rather than in one entrypoint that the other cannot reach.
//!
//! The pieces come back owned because the fill's inputs borrow them: the
//! executor holds `&account_map` and `&entries`, and the book refs point into
//! the levels each quote returned. A function that built the executor itself
//! would be returning references to its own locals, so the caller keeps this
//! struct alive and borrows from it ([`Self::book_refs`], [`Self::executor`]).

use {
    super::cpi_executor::CpiQuoterExecutor,
    crate::{
        error::ErrorCode,
        math::router::QuoterBook,
        state::prop_amm::{
            ClobUserRefV0, Direction, PriceLevel, QuoteArgsV0, QuoterType, QuoterUserSetRef,
            QuoterV0,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    std::collections::BTreeMap,
};

pub struct QuoterSection<'info> {
    /// Every account past the map/user sections, by key — what a quoter CPI's
    /// registered account list is resolved against.
    pub account_map: BTreeMap<Pubkey, AccountInfo<'info>>,
    /// The entries that actually quoted, in book order. Inactive or
    /// unapproved entries are dropped here, so index `i` is book `i`.
    pub entries: Vec<AccountLoader<'info, QuoterV0>>,
    pub types: Vec<QuoterType>,
    pub quoter_users: Vec<Pubkey>,
    pub response_accounts: Vec<Pubkey>,
    /// `(priority, levels)` per quoting entry, parallel to `entries`.
    pub books: Vec<(u8, Vec<PriceLevel>)>,
    /// Every entry the caller passed, quoting or not — what the mandatory
    /// baseline is checked against, since a dead entry still satisfies it.
    pub passed: Vec<Pubkey>,
}

/// What the section needs to quote: the taker's side and size, plus the
/// identities forwarded on the wire.
pub struct QuoteInputs<'a> {
    pub market_index: u16,
    pub direction: Direction,
    pub size: u64,
    /// The loaded-user set quoters must not fill outside of.
    pub users: &'a [ClobUserRefV0],
    pub taker: ClobUserRefV0,
    pub quoter_signer: Pubkey,
    pub quoter_signer_nonce: u8,
}

impl<'info> QuoterSection<'info> {
    /// Classify the leftover accounts, then quote every live entry among them.
    ///
    /// Deactivated or unapproved entries are skipped rather than rejected: a
    /// route signed before an admin pulled approval must not brick the fill.
    /// A market mismatch or a duplicate is a malformed transaction and fails
    /// loudly.
    pub fn quote(
        leftover: &[&'info AccountInfo<'info>],
        inputs: &QuoteInputs<'_>,
    ) -> Result<QuoterSection<'info>> {
        let account_map: BTreeMap<Pubkey, AccountInfo<'info>> = leftover
            .iter()
            .map(|info| (*info.key, (*info).clone()))
            .collect();
        let candidates: Vec<AccountLoader<'info, QuoterV0>> = leftover
            .iter()
            .filter(|info| {
                info.owner == &crate::ID
                    && info
                        .try_borrow_data()
                        .is_ok_and(|data| data.get(..8) == Some(QuoterV0::DISCRIMINATOR))
            })
            .map(|info| AccountLoader::try_from(*info))
            .collect::<Result<_>>()?;

        let mut section = QuoterSection {
            account_map,
            entries: Vec::with_capacity(candidates.len()),
            types: Vec::with_capacity(candidates.len()),
            quoter_users: Vec::with_capacity(candidates.len()),
            response_accounts: Vec::with_capacity(candidates.len()),
            books: Vec::with_capacity(candidates.len()),
            passed: Vec::with_capacity(candidates.len()),
        };

        for loader in candidates {
            let quoted = {
                let quoter = loader.load()?;
                validate!(
                    quoter.market == inputs.market_index,
                    ErrorCode::DefaultError,
                    "quoter entry {} is for market {}, fill is for market {}",
                    loader.key(),
                    quoter.market,
                    inputs.market_index
                )?;
                validate!(
                    quoter.quoter_type != QuoterType::Vamm,
                    ErrorCode::DefaultError,
                    "the vAMM quotes in-program, not through the registry"
                )?;
                validate!(
                    !section.passed.contains(&loader.key()),
                    ErrorCode::DefaultError,
                    "duplicate quoter entry {}",
                    loader.key()
                )?;
                section.passed.push(loader.key());
                if !(quoter.is_active && quoter.is_approved) {
                    continue;
                }
                let levels = quoter.quote(
                    inputs.market_index,
                    QuoteArgsV0 {
                        direction: inputs.direction,
                        size: inputs.size,
                        users: QuoterUserSetRef(inputs.users),
                        taker: Some(inputs.taker),
                    },
                    &inputs.quoter_signer,
                    inputs.quoter_signer_nonce,
                    &section.account_map,
                )?;
                (
                    quoter.priority,
                    quoter.quoter_type,
                    quoter.user,
                    quoter.response_account,
                    levels,
                )
            };
            let (priority, quoter_type, quoter_user, response_account, levels) = quoted;
            section.entries.push(loader);
            section.types.push(quoter_type);
            section.quoter_users.push(quoter_user);
            section.response_accounts.push(response_account);
            section.books.push((priority, levels));
        }
        Ok(section)
    }

    /// A route can't exclude the public book: when the market names a
    /// canonical CLOB entry, the transaction must carry it. A dead entry
    /// satisfies this — it was passed and skipped at quote time — so killing a
    /// book never bricks fills. The vAMM half of the baseline is inherent:
    /// it's in-program, gated only by oracle validity.
    pub fn require_baseline(&self, required_clob: Pubkey) -> Result<()> {
        validate!(
            required_clob == Pubkey::default() || self.passed.contains(&required_clob),
            ErrorCode::DefaultError,
            "router fill must include the market's CLOB quoter {}",
            required_clob
        )?;
        Ok(())
    }

    pub fn book_refs(&self) -> Vec<QuoterBook<'_>> {
        self.books
            .iter()
            .map(|(priority, levels)| QuoterBook {
                priority: *priority,
                levels,
            })
            .collect()
    }

    pub fn executor<'a>(
        &'a self,
        inputs: &QuoteInputs<'_>,
        slot: u64,
        now: i64,
    ) -> CpiQuoterExecutor<'a, 'info> {
        CpiQuoterExecutor {
            quoters: &self.entries,
            types: self.types.clone(),
            quoter_users: self.quoter_users.clone(),
            response_accounts: self.response_accounts.clone(),
            market_index: inputs.market_index,
            account_map: &self.account_map,
            quoter_signer: inputs.quoter_signer,
            quoter_signer_nonce: inputs.quoter_signer_nonce,
            users: inputs.users.to_vec(),
            taker: inputs.taker,
            slot,
            now,
        }
    }
}
