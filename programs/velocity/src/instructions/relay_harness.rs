//! The shared shape of every relay resolver, so a new resolver holds only its
//! discovery logic.
//!
//! Every resolver looks for work. If there is none it says so. Otherwise it
//! describes the executor call, meaning its account list and its args, writes
//! that into the shared scratch account, and returns a pointer to it. Only the
//! first step differs between resolvers. [`resolve_into`] owns the rest.
//!
//! The builder also enforces relay's two rules about staged executors.
//!
//! - The keeper placeholder must appear. Relay substitutes its payout account
//!   for [`KEEPER_PLACEHOLDER`]. An executor that never names it leaves the
//!   payment guard asserting against a stranger's balance, and the turner
//!   rejects the resolver output.
//! - No account may be a signer. The turner marks every executor meta
//!   non-signing, and it refuses to sign a transaction whose executor names a
//!   signer. An executor is permissionless, so a signing account handed to one
//!   can be drained. An anchor `Signer`, and an `init` or `init_if_needed`
//!   that needs a payer, therefore cannot appear in an instruction relay
//!   stages.
//!
//! Both checks live here rather than in review. A resolver that breaks one
//! fails its own simulation with a named error, instead of being skipped by
//! every turner with no message.
//!
//! ## The executor is the resolver's answer
//!
//! A condition names only its resolver. The instruction to run comes back
//! inside the staged payload. A [`StagedCall`] is therefore built from both of
//! the executor's generated types, `crate::instruction::X` for its
//! discriminator and `crate::accounts::X` for its account list.
//! [`staged_call!`] pairs the two. Naming the executor here rather than in the
//! condition means arming a crank does not restate an identity its resolver
//! already knows.
//!
//! ## The fired-condition identity is ignored
//!
//! Relay appends a [`relay_spec::FiredConditionV0`] to every resolver's
//! instruction data. It holds the target account, the block offset and the
//! slot index. Velocity's resolvers do not declare it, and anchor's borsh
//! dispatch ignores the trailing bytes. For these blocks the identity is
//! redundant, and trusting it would be worse than scanning.
//!
//! - Each condition kind has its own resolver, so the discriminator relay
//!   dispatched already says what kind of work is due.
//! - The sync instructions rewrite a whole block in place, so a slot index
//!   moves under a resync while the watch that fired keeps its coordinates. A
//!   resolver that answered only for the index it was handed would answer
//!   about whatever moved into that slot. One that re-derives the due work
//!   from the accounts it holds cannot.
//!
//! A resolver that needs the identity, such as one shared by several slots of
//! the same block, must declare it as an argument and validate it against the
//! account it loaded. Relay's docs require that. The identity is an argument,
//! and it grants no capability.

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

/// An executor call a resolver decided on. It holds the instruction to run,
/// its account list, and the borsh args that follow the discriminator.
pub struct StagedCall {
    disc: &'static [u8],
    metas: Vec<AccountMeta>,
    data: Vec<u8>,
}

impl StagedCall {
    /// `I` is `crate::instruction::X` and `accounts` is `crate::accounts::X`. Relay invokes the
    /// discriminator of `I`. A rename or an `#[derive(Accounts)]` change then breaks staging at
    /// compile time, and the writability flags come from the derive rather than from hand-written
    /// metas. Prefer [`staged_call!`], which pairs the two types from one name.
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

    /// Append a `(User, UserStats)` pair. Both are writable, as every
    /// settlement path expects.
    pub fn user_pair(self, user: Pubkey, stats: Pubkey) -> Self {
        self.account(user, true).account(stats, true)
    }

    /// The margin-map section for an executor that names the perp market in
    /// its own accounts struct. It appends the oracle as readonly, then the
    /// quote spot market. The order matters, as it does in
    /// [`Self::map_section`].
    pub fn map_section_named_perp(self, oracle: Pubkey, quote_spot_market_index: u16) -> Self {
        self.account(oracle, false)
            .account(pdas::spot_market(quote_spot_market_index), true)
    }

    /// Append the margin-map section every executor's `load_maps` call parses.
    /// It holds the oracle as readonly, the quote spot market, then the perp
    /// market. `load_maps` reads these by order rather than by name, so the
    /// ordering lives here once instead of in each resolver.
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
    /// book. A CLOB node carries `(authority, sub_account_id)`, which is what
    /// makes the derivation possible.
    pub fn maker_refs(
        self,
        makers: impl IntoIterator<Item = crate::state::prop_amm::ClobUserRefV0>,
    ) -> Self {
        makers.into_iter().fold(self, |call, maker| {
            let (user, stats) = pdas::user_pair(&maker.authority, maker.sub_account_id);
            call.user_pair(user, stats)
        })
    }

    /// Append account refs captured earlier, such as a stored margin-map
    /// section or a registered CPI surface.
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

/// Run a resolver. `discover` returns the executor call, or `None` for no
/// work. This function does everything else: the no-work response, the
/// account-ref conversion and its rule checks, the staging, and the return
/// data.
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
