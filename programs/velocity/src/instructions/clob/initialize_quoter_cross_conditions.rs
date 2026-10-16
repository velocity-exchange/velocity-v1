//! Create a Custom quoter's relay cross-discovery conditions, or re-price
//! conditions that already exist.
//!
//! The instruction is permissionless. Every input is checked against the
//! registry and the market, and the caller only pays the rent. Only the
//! maker's authority chooses the fallback poll interval. Any other caller
//! attaches at the default and keeps a stored interval on a re-attach. An attach
//! requires the entry to be active and approved, and requires the market's
//! canonical CLOB to be attached. The conditions PDA derives from the entry
//! key. A repeat call re-prices in place, which covers a new keeper payment, a
//! changed watch declaration, and a rotated CLOB. The market attach is
//! idempotent in the same way.

use {
    crate::{
        error::ErrorCode,
        state::{
            clob_crank::{ClobCrankConditionsV0, CLOB_CRANK_CONDITIONS_PDA_SEED},
            perp_market::PerpMarket,
            prop_amm::{
                clob_slot_index, slot_for_entry, QuoterConfigV0, QuoterSlabExt, QuoterSlabV0,
                QuoterType, QuoterV0,
            },
            quoter_cross::{
                QuoterCrossConditionsV0, QUOTER_CROSS_CLOB, QUOTER_CROSS_CONDITIONS_PDA_SEED,
                QUOTER_CROSS_FALLBACK, QUOTER_CROSS_WATCH,
            },
            state::State,
        },
        validate,
    },
    anchor_lang::{prelude::*, Discriminator},
    relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0},
    std::convert::TryInto,
};

#[derive(Accounts)]
pub struct InitializeQuoterCrossConditions<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub state: AccountLoader<'info, State>,
    /// The Custom entry to discover crosses for. The conditions PDA derives
    /// from it. Its live config comes from the slab rather than from here.
    pub quoter: AccountLoader<'info, QuoterV0>,
    #[account(
        seeds = [b"perp_market", quoter.load()?.config.market.to_le_bytes().as_ref()],
        bump,
        has_one = quoter_slab
    )]
    pub perp_market: AccountLoader<'info, PerpMarket>,
    /// The market's slab. It holds the entry's approved config, and the
    /// book's config at slot 0. The book is the other leg of every staged
    /// cross. The market's `has_one` binds the slab, which costs a memcmp where
    /// a seeds constraint would pay for a PDA derivation.
    #[account()]
    pub quoter_slab: AccountLoader<'info, QuoterSlabV0>,
    /// The market's crank conditions, which are the source of truth for the
    /// keeper payment.
    #[account(
        seeds = [
            CLOB_CRANK_CONDITIONS_PDA_SEED,
            quoter.load()?.config.market.to_le_bytes().as_ref(),
        ],

        bump
    )]
    pub market_conditions: AccountLoader<'info, ClobCrankConditionsV0>,
    #[account(
        init_if_needed,
        seeds = [QUOTER_CROSS_CONDITIONS_PDA_SEED, quoter.key().as_ref()],
        space = QuoterCrossConditionsV0::SIZE,
        bump,
        payer = payer
    )]
    pub cross_conditions: AccountLoader<'info, QuoterCrossConditionsV0>,
    pub rent: Sysvar<'info, Rent>,
    pub system_program: Program<'info, System>,
}

/// Ceiling on the cross-discovery poll interval, roughly an hour of slots.
pub const QUOTER_CROSS_FALLBACK_MAX_SLOTS: u64 = 9_000;

/// The interval an attach by anyone other than the maker's authority uses.
/// Roughly ten minutes of slots.
pub const QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS: u64 = 1_500;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize)]
pub struct InitializeQuoterCrossConditionsArgs {
    /// The poll interval behind the reprice watch. It is the discovery floor
    /// when the maker's declared watch misses a reprice. Bounded above by
    /// [`QUOTER_CROSS_FALLBACK_MAX_SLOTS`]. A caller other than the maker's
    /// authority must pass [`QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS`].
    pub expire_fallback_slots: u64,
}

pub fn handle_initialize_quoter_cross_conditions(
    ctx: Context<InitializeQuoterCrossConditions>,
    args: InitializeQuoterCrossConditionsArgs,
) -> Result<()> {
    CrossConditionsWrite {
        state: ctx.accounts.state.key(),
        quoter: ctx.accounts.quoter.key(),
        caller: ctx.accounts.payer.key(),
        perp_market: &ctx.accounts.perp_market,
        quoter_slab: &ctx.accounts.quoter_slab,
        market: MarketCrossTerms::from_conditions(&*ctx.accounts.market_conditions.load()?)?,
        cross_conditions: &ctx.accounts.cross_conditions,
    }
    .write(Some(args.expire_fallback_slots))
}

/// The accounts that create or re-price one entry's cross conditions. The
/// caller binds each one to the entry's market and to the entry key.
pub struct CrossConditionsWrite<'a, 'info> {
    pub state: Pubkey,
    pub quoter: Pubkey,
    pub caller: Pubkey,
    pub perp_market: &'a AccountLoader<'info, PerpMarket>,
    pub quoter_slab: &'a AccountLoader<'info, QuoterSlabV0>,
    pub market: MarketCrossTerms,
    pub cross_conditions: &'a AccountLoader<'info, QuoterCrossConditionsV0>,
}

/// What the market's crank conditions give every cross condition.
pub struct MarketCrossTerms {
    keeper_payment_lamports: u64,
    top_of_book_offset: u32,
    top_of_book_len: u32,
    quote_oracle: Pubkey,
}

impl MarketCrossTerms {
    /// The resolver stages `crank_cross_match` and nothing else, so the floor
    /// is that crank's payment out of the market's reservoir. The book
    /// reported its top-of-book region when the market attached.
    pub fn from_conditions(market_conditions: &ClobCrankConditionsV0) -> Result<Self> {
        validate!(
            market_conditions.top_of_book_len > 0,
            ErrorCode::InvalidQuoterConfig,
            "market conditions carry no top-of-book region; re-run the market's attach"
        )?;

        Ok(Self {
            keeper_payment_lamports: u64::from(market_conditions.crank_payments.cross),
            top_of_book_offset: market_conditions.top_of_book_offset,
            top_of_book_len: market_conditions.top_of_book_len,
            quote_oracle: market_conditions.quote_oracle,
        })
    }
}

impl CrossConditionsWrite<'_, '_> {
    /// Write the block. `requested_fallback_slots` is `None` when the caller
    /// asks for no interval, and the block then keeps its stored one.
    pub fn write(&self, requested_fallback_slots: Option<u64>) -> Result<()> {
        let slots = self.quoter_slab.slots()?;
        let quoter_slot = slot_for_entry(&slots, &self.quoter).ok_or_else(|| {
            msg!("quoter holds no slab slot; approve it first");
            error!(ErrorCode::QuoterNotOnSlab)
        })?;
        let quoter = &slots[quoter_slot].config;
        validate!(
            quoter.quoter_type == QuoterType::Custom,
            ErrorCode::InvalidQuoterConfig,
            "cross conditions are for Custom quoters (the CLOB's are on the market conditions)"
        )?;
        validate!(
            slots[quoter_slot].quotes(),
            ErrorCode::InvalidQuoterConfig,
            "quoter must be active and approved to attach cross discovery"
        )?;

        let book_slot = clob_slot_index(&slots).ok_or_else(|| {
            msg!("quoter slab holds no book slot");
            error!(ErrorCode::QuoterNotOnSlab)
        })?;
        let clob = &slots[book_slot].config;
        validate!(
            self.perp_market.load()?.clob_market == clob.response_account,
            ErrorCode::InvalidQuoterConfig,
            "the slab's book is not the market's canonical book"
        )?;

        let terms = &self.market;
        let resolver_accounts = self.resolver_accounts(quoter, clob)?;
        let disc8 = |disc: &[u8]| -> Result<[u8; 8]> {
            disc.try_into()
                .map_err(|_| error!(ErrorCode::CastingFailure))
        };
        let spec = CrankSpecV0 {
            resolver_program: crate::ID.to_bytes(),
            resolver_disc: disc8(crate::instruction::ResolveCrankCrossMatchQuoter::DISCRIMINATOR)?,
            min_payment: terms.keeper_payment_lamports,
        };

        let mut conditions = match self.cross_conditions.load_init() {
            Ok(conditions) => conditions,
            // The account already exists on a re-attach, so re-price in place.
            Err(_) => self.cross_conditions.load_mut()?,
        };
        let expire_fallback_slots = fallback_interval(
            requested_fallback_slots,
            self.caller == quoter.authority,
            conditions.fallback_slots(),
        )?;

        let (oracle, quote_spot_market_index) = {
            let market = self.perp_market.load()?;
            (market.oracle, market.quote_spot_market_index)
        };
        conditions.quoter = self.quoter;
        conditions.clob_market = clob.response_account;
        conditions.clob_program = clob.program_id;
        conditions.oracle = oracle;
        conditions.market_index = quoter.market;
        conditions.quote_spot_market_index = quote_spot_market_index;
        conditions.quote_oracle = terms.quote_oracle;
        conditions.init_block()?;
        let resolvers = conditions.write_resolver_list(&resolver_accounts)?;

        set_cross_conditions(
            &mut conditions,
            quoter,
            &clob.response_account,
            terms,
            spec,
            resolvers,
            expire_fallback_slots,
        )
    }

    /// The resolver's account list, in order: the shared scratch, the
    /// conditions, the book, the state, the slab, the entry's quoted user, the
    /// CLOB program, then the entry's registered quote accounts and its
    /// program. Each condition points at this one list. The book is writable
    /// because the resolver streams its quote into the book's response tail.
    fn resolver_accounts(
        &self,
        quoter: &QuoterConfigV0,
        clob: &QuoterConfigV0,
    ) -> Result<Vec<AccountRefV0>> {
        let mut resolver_accounts = vec![
            AccountRefV0::writable(crate::state::pdas::relay_scratch().to_bytes()),
            AccountRefV0::readonly(self.cross_conditions.key().to_bytes()),
            AccountRefV0::writable(clob.response_account.to_bytes()),
            AccountRefV0::readonly(self.state.to_bytes()),
            AccountRefV0::readonly(self.quoter_slab.key().to_bytes()),
            AccountRefV0::readonly(quoter.user.to_bytes()),
            AccountRefV0::readonly(clob.program_id.to_bytes()),
        ];

        for meta in quoter.leg_metas(quoter.quote_leg_indexes())? {
            resolver_accounts.push(if meta.is_writable {
                AccountRefV0::writable(meta.pubkey.to_bytes())
            } else {
                AccountRefV0::readonly(meta.pubkey.to_bytes())
            });
        }

        resolver_accounts.push(AccountRefV0::readonly(quoter.program_id.to_bytes()));
        Ok(resolver_accounts)
    }
}

fn set_cross_conditions(
    conditions: &mut QuoterCrossConditionsV0,
    quoter: &QuoterConfigV0,
    clob_market: &Pubkey,
    terms: &MarketCrossTerms,
    spec: CrankSpecV0,
    resolvers: relay_spec::ResolverListV0,
    expire_fallback_slots: u64,
) -> Result<()> {
    // The maker-declared reprice watch. It stays inactive when the maker
    // declares nothing. The fallback poll is then the only wake for this
    // side.
    if quoter.watch_len > 0 {
        conditions.set_condition(
            QUOTER_CROSS_WATCH,
            &(ConditionV0::on_account_change(
                relay_spec::WatchedRegion::new(
                    quoter.watch_account.to_bytes(),
                    quoter.watch_offset,
                    quoter.watch_len,
                ),
                spec,
                resolvers,
            )),
        )?;
    } else {
        conditions.set_condition(
            QUOTER_CROSS_WATCH,
            &relay_spec::bytemuck::Zeroable::zeroed(),
        )?;
    }

    conditions.set_condition(
        QUOTER_CROSS_CLOB,
        // The book's own top-of-book region. A crossing order is always a new
        // best, so a change there covers every cross this entry could take.
        &(ConditionV0::on_account_change(
            relay_spec::WatchedRegion::new(
                clob_market.to_bytes(),
                terms.top_of_book_offset,
                terms.top_of_book_len,
            ),
            spec,
            resolvers,
        )),
    )?;
    conditions.set_condition(
        QUOTER_CROSS_FALLBACK,
        &(ConditionV0::every_slots(expire_fallback_slots, spec, resolvers)),
    )
}

/// The fallback interval an attach writes.
///
/// A longer interval delays cross discovery for the maker's quotes, so only
/// the maker's authority chooses one. Any other caller must ask for the
/// default. No request keeps whatever interval the block already holds, and
/// so does a re-attach by another caller.
fn fallback_interval(
    requested: Option<u64>,
    caller_is_maker: bool,
    stored: Option<u64>,
) -> Result<u64> {
    let Some(requested) = requested else {
        return Ok(stored.unwrap_or(QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS));
    };

    validate!(
        requested > 0,
        ErrorCode::InvalidQuoterConfig,
        "fallback interval must be nonzero"
    )?;
    validate!(
        requested <= QUOTER_CROSS_FALLBACK_MAX_SLOTS,
        ErrorCode::InvalidQuoterConfig,
        "fallback interval {} is past the {} slot ceiling",
        requested,
        QUOTER_CROSS_FALLBACK_MAX_SLOTS
    )?;

    if caller_is_maker {
        return Ok(requested);
    }

    validate!(
        requested == QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS,
        ErrorCode::InvalidQuoterConfig,
        "only the maker's authority sets a fallback interval other than {}",
        QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS
    )?;

    Ok(stored.unwrap_or(QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS))
}

#[cfg(test)]
mod tests {
    use super::{
        fallback_interval, QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS, QUOTER_CROSS_FALLBACK_MAX_SLOTS,
    };

    /// A third party cannot widen a maker's poll interval up to the ceiling.
    #[test]
    fn a_third_party_cannot_choose_the_interval() {
        assert!(fallback_interval(Some(QUOTER_CROSS_FALLBACK_MAX_SLOTS), false, None).is_err());
        assert_eq!(
            fallback_interval(Some(QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS), false, None).unwrap(),
            QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS
        );
    }

    /// A third party re-attach keeps the interval the maker chose.
    #[test]
    fn a_third_party_reattach_keeps_the_stored_interval() {
        assert_eq!(
            fallback_interval(Some(QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS), false, Some(300)).unwrap(),
            300
        );
    }

    /// An attach that asks for no interval keeps the stored one, and a new
    /// block starts at the default.
    #[test]
    fn no_request_keeps_the_stored_interval() {
        assert_eq!(fallback_interval(None, true, Some(300)).unwrap(), 300);
        assert_eq!(
            fallback_interval(None, false, None).unwrap(),
            QUOTER_CROSS_FALLBACK_DEFAULT_SLOTS
        );
    }

    #[test]
    fn the_maker_chooses_any_interval_up_to_the_ceiling() {
        assert_eq!(
            fallback_interval(Some(300), true, Some(9_000)).unwrap(),
            300
        );
        assert!(fallback_interval(Some(QUOTER_CROSS_FALLBACK_MAX_SLOTS + 1), true, None).is_err());
        assert!(fallback_interval(Some(0), true, None).is_err());
    }
}
