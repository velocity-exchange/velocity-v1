//! The shared shape of every relay resolver, so a new one is its discovery
//! logic and nothing else.
//!
//! A resolver is always: look for work; if there is none say so; otherwise
//! describe the executor call (its account list and its args), write that
//! into the shared scratch account, and return a pointer to it. Only the
//! first step differs between resolvers — [`resolve_into`] owns the rest.
//!
//! The builder is also where relay's two hard rules about staged executors
//! are enforced, because both have already been tripped once by hand:
//!
//! - **The keeper placeholder must appear.** Relay substitutes its payout
//!   account for [`KEEPER_PLACEHOLDER`]; an executor that never names it
//!   leaves the payment guard asserting against a stranger's balance, and
//!   the turner rejects the resolver output.
//! - **No account may be a signer.** The turner marks every executor meta
//!   non-signing *and* refuses to sign a transaction whose executor names
//!   a signer, because executors are permissionless and a signing account
//!   handed to one is a drain vector. In practice this means an anchor
//!   `Signer` — or an `init`/`init_if_needed` that needs a payer — cannot
//!   appear in an instruction relay stages.
//!
//! Both are checked here rather than left to review, so a resolver that
//! violates one fails its own simulation with a named error instead of
//! being silently skipped forever by turners.
//!
//! ## The executor is the resolver's answer
//!
//! A condition names only its resolver; the instruction to actually run
//! comes back inside the staged payload. So a [`StagedCall`] is built from
//! *both* of the executor's generated types — `crate::instruction::X` for
//! its discriminator and `crate::accounts::X` for its account list — which
//! is what [`staged_call!`] exists to pair. Naming the executor here rather
//! than in the condition means arming a crank no longer has to restate an
//! identity its resolver already knows.
//!
//! ## The fired-condition identity is deliberately ignored
//!
//! Relay appends a [`relay_spec::FiredConditionV0`] (target account, block
//! offset, slot index) to every resolver's instruction data. Velocity's
//! resolvers do not declare it — anchor's borsh dispatch ignores trailing
//! bytes — because for these blocks it is redundant, and trusting it would
//! be worse than scanning:
//!
//! - Each condition kind has its own resolver, so the discriminator relay
//!   dispatched already says what kind of work is due.
//! - The sync instructions rewrite a whole block in place, so a slot index
//!   moves under a re-sync while the watch that fired keeps its coordinates.
//!   A resolver that answered only for the index it was handed would answer
//!   about whatever moved into that slot; one that re-derives the due work
//!   from the accounts it holds cannot.
//!
//! A resolver that ever *needs* the identity (a block whose slots share one
//! resolver) should declare it as an argument and validate it against the
//! account it loaded, exactly as relay's docs require — it is an argument,
//! not a capability.

use {
    crate::{
        error::ErrorCode,
        msg,
        state::{pdas, relay_scratch::RelayScratchV0},
    },
    anchor_lang::prelude::*,
    relay_spec::{AccountRefV0, ResolvedCrankV0, ResponsePointerV0, KEEPER_PLACEHOLDER},
    solana_program::{instruction::AccountMeta, program::set_return_data},
    std::ops::DerefMut,
};

/// An executor call a resolver has decided on: which instruction to run,
/// its account list, and the borsh args that follow the discriminator.
pub struct StagedCall {
    disc: &'static [u8],
    metas: Vec<AccountMeta>,
    data: Vec<u8>,
}

impl StagedCall {
    /// Name the executor by its two generated types: `I` is
    /// `crate::instruction::X` (the discriminator relay will invoke) and
    /// `accounts` is `crate::accounts::X`, so a change to either the
    /// instruction's name or its `#[derive(Accounts)]` shape breaks staging
    /// at compile time — and the writability flags come from the derive
    /// rather than being restated by hand. Prefer [`staged_call!`], which
    /// pairs the two from one name.
    pub fn new<I: anchor_lang::Discriminator>(accounts: impl ToAccountMetas) -> Self {
        Self {
            disc: I::DISCRIMINATOR,
            metas: accounts.to_account_metas(None),
            data: Vec::new(),
        }
    }

    /// Append one account.
    pub fn account(mut self, pubkey: Pubkey, writable: bool) -> Self {
        self.metas.push(if writable {
            AccountMeta::new(pubkey, false)
        } else {
            AccountMeta::new_readonly(pubkey, false)
        });
        self
    }

    /// Append a `(User, UserStats)` pair — both writable, as every
    /// settlement path expects.
    pub fn user_pair(self, user: Pubkey, stats: Pubkey) -> Self {
        self.account(user, true).account(stats, true)
    }

    /// The margin-map section for an executor that names the perp market in
    /// its own accounts struct: oracle (readonly), then the quote spot
    /// market. **Positional**, like [`Self::map_section`].
    pub fn map_section_named_perp(self, oracle: Pubkey, quote_spot_market_index: u16) -> Self {
        self.account(oracle, false)
            .account(pdas::spot_market(quote_spot_market_index), true)
    }

    /// Append the margin-map section every executor's `load_maps` call
    /// parses: oracle (readonly), the quote spot market, then the perp
    /// market. **Positional** — `load_maps` reads these by order, not by
    /// name, so the ordering lives here once instead of in each resolver.
    pub fn map_section(
        self,
        oracle: Pubkey,
        quote_spot_market_index: u16,
        perp_market_index: u16,
    ) -> Self {
        self.account(oracle, false)
            .account(pdas::spot_market(quote_spot_market_index), true)
            .account(pdas::perp_market(perp_market_index), true)
    }

    /// Append the `(User, UserStats)` pairs of maker identities read off a
    /// book — the derivation the CLOB's `(authority, sub_account_id)`
    /// nodes exist to make possible.
    pub fn maker_refs(
        self,
        makers: impl IntoIterator<Item = crate::state::prop_amm::ClobUserRefV0>,
    ) -> Self {
        makers.into_iter().fold(self, |call, maker| {
            let (user, stats) = pdas::user_pair(&maker.authority, maker.sub_account_id);
            call.user_pair(user, stats)
        })
    }

    /// Append account refs captured earlier (a stored margin-map section,
    /// a registered CPI surface).
    pub fn refs(mut self, refs: impl IntoIterator<Item = AccountRefV0>) -> Self {
        for r in refs {
            self.metas.push(if r.writable != 0 {
                AccountMeta::new(Pubkey::new_from_array(r.address), false)
            } else {
                AccountMeta::new_readonly(Pubkey::new_from_array(r.address), false)
            });
        }
        self
    }

    /// Append one borsh arg, in the executor's parameter order.
    pub fn arg(mut self, value: impl AnchorSerialize) -> Result<Self> {
        value
            .serialize(&mut self.data)
            .map_err(|_| ErrorCode::DefaultError)?;
        Ok(self)
    }

    fn into_resolved(self) -> Result<ResolvedCrankV0> {
        let executor_disc: [u8; 8] = std::convert::TryInto::<[u8; 8]>::try_into(self.disc)
            .map_err(|_| {
                msg!("staged executor discriminator is not 8 bytes");
                error!(ErrorCode::DefaultError)
            })?;
        let mut names_placeholder = false;
        let accounts = self
            .metas
            .iter()
            .map(|meta| {
                if meta.is_signer {
                    msg!("staged executor named a signer: {}", meta.pubkey);
                    return Err(error!(ErrorCode::DefaultError));
                }
                if meta.pubkey.to_bytes() == KEEPER_PLACEHOLDER {
                    names_placeholder = true;
                }
                Ok(if meta.is_writable {
                    AccountRefV0::writable(meta.pubkey.to_bytes())
                } else {
                    AccountRefV0::readonly(meta.pubkey.to_bytes())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !names_placeholder {
            msg!("staged executor names no keeper placeholder");
            return Err(error!(ErrorCode::DefaultError));
        }
        Ok(ResolvedCrankV0::new(
            crate::ID.to_bytes(),
            executor_disc,
            accounts,
            self.data,
        ))
    }
}

/// Run a resolver: `discover` returns the executor call, or `None` for no
/// work. Everything else — the no-work response, the account-ref
/// conversion and its rule checks, staging, and the return data — happens
/// here.
pub fn resolve_into<'info>(
    scratch: &AccountLoader<'info, RelayScratchV0>,
    discover: impl FnOnce() -> Result<Option<StagedCall>>,
) -> Result<()> {
    let Some(call) = discover()? else {
        set_return_data(&ResponsePointerV0::no_work().to_bytes());
        return Ok(());
    };
    let resolved = call.into_resolved()?;
    let pointer = scratch.load_mut()?.deref_mut().stage(&resolved)?;
    set_return_data(&pointer);
    Ok(())
}

/// Narrow a generated discriminator to the width a crank spec holds.
///
/// Every resolver names its executor through a generated `DISCRIMINATOR`
/// constant, which is a slice. A crank spec holds eight bytes.
pub fn disc8(disc: &[u8]) -> Result<[u8; 8]> {
    std::convert::TryInto::<[u8; 8]>::try_into(disc).map_err(|_| error!(ErrorCode::DefaultError))
}

/// Stage a call to one of velocity's own executors, named once.
///
/// `staged_call!(TriggerOrder { state, user, .. })` expands to a
/// [`StagedCall`] carrying `crate::instruction::TriggerOrder`'s
/// discriminator and `crate::accounts::TriggerOrder`'s metas. The two
/// generated types share the executor's name, and pairing them here is what
/// keeps a resolver from staging one instruction's accounts under another's
/// discriminator.
#[macro_export]
macro_rules! staged_call {
    ($executor:ident $accounts:tt) => {
        $crate::instructions::StagedCall::new::<$crate::instruction::$executor>(
            $crate::accounts::$executor $accounts,
        )
    };
}
