//! Quoter registry: [`QuoterV0`] entries name an external quoter program
//! (CLOB, Midpoint, custom PropAMMs; the vAMM is in-program) plus the CPI
//! surface velocity needs to call it — discriminators, account lists, and the
//! response account. [`QuoterV0::quote`]/[`QuoterV0::execute`] are the CPI
//! legs the router fill uses. Registration ixs live in
//! `instructions::quoter_registry`.

use std::collections::BTreeMap;

use anchor_lang::prelude::*;
use solana_program::{
    instruction::{AccountMeta, Instruction},
    program::{get_return_data, invoke_signed},
};
use static_assertions::const_assert_eq;

use crate::{error::ErrorCode, msg, signer::get_signer_seeds, state::traits::Size, validate};

/// Max accounts that can be registered per CPI leg (quote / execute).
pub const MAX_QUOTER_ACCOUNTS: usize = 32;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum QuoterType {
    Vamm,
    Clob,
    #[default]
    Custom,
}

#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug, Default)]
#[repr(C)]
pub struct QuoterV0 {
    /// For Custom quoters, the User this quoter is allowed to quote for.
    /// That user's authority creates the entry, so creation is consent. For
    /// vAMM, the vAMM user. For CLOB, ignored: execute may return balance
    /// changes for any user with resting orders on the CLOB.
    pub user: Pubkey,
    /// The external program invoked for `quote_v0` / `execute_v0`.
    pub program_id: Pubkey,
    /// Account owned by `program_id` that quote/execute responses are written
    /// into; must be registered in both account lists. Responses are read at
    /// the pointer returned via return data, so payloads aren't bound by the
    /// 1024-byte return-data cap.
    pub response_account: Pubkey,
    /// Manages this registry entry. For Custom quoters this is the quoted
    /// user's authority (enforced at creation, no handoff), so the maker can
    /// always kill their own quoter (`is_active`); the admin vets the CPI
    /// surface (`is_approved`), which any config change resets.
    pub authority: Pubkey,
    /// Raw instruction discriminators on `program_id`. Stored rather than
    /// derived so non-Anchor programs can participate.
    pub quote_v0_discriminator: [u8; 8],
    pub execute_v0_discriminator: [u8; 8],
    /// Accounts forwarded to `quote_v0`, in order. Only the first
    /// `quote_accounts_count` entries are live.
    pub quote_accounts: [AmmAccountMeta; MAX_QUOTER_ACCOUNTS],
    /// Accounts forwarded to `execute_v0`, in order. Only the first
    /// `execute_accounts_count` entries are live.
    pub execute_accounts: [AmmAccountMeta; MAX_QUOTER_ACCOUNTS],
    /// Perp market index this quoter serves.
    pub market: u16,
    pub quoter_type: QuoterType,
    /// The authority's own on/off switch — always settable by the maker.
    pub is_active: bool,
    /// Admin vetting of the CPI surface; reset by any config change.
    pub is_approved: bool,
    pub quote_accounts_count: u8,
    pub execute_accounts_count: u8,
    pub padding: [u8; 9],
}

// Zero-copy layout invariant (see docs/alignment-and-native-offsets.md):
// no u128 fields, size (incl. 8-byte discriminator) ≡ 8 (mod 16).
const_assert_eq!(std::mem::size_of::<QuoterV0>(), 2720);
const_assert_eq!((QuoterV0::SIZE - 8) % 16, 0);

impl Size for QuoterV0 {
    const SIZE: usize = 2728;
}

/// PDA: one entry per (perp market, quoter program, quoted user).
pub const QUOTER_PDA_SEED: &[u8] = b"quoter";

/// Which CPI leg an account-list update targets.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub enum QuoterCpiLeg {
    Quote,
    Execute,
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct AmmAccountMeta {
    pub pubkey: Pubkey,
    /// Whether the account is passed writable to the quoter program.
    /// `is_signer` is intentionally not stored: quoter CPIs never receive
    /// signer privilege at all (see `invoke_quoter`).
    pub is_writable: bool,
    pub padding: [u8; 7],
}

const_assert_eq!(std::mem::size_of::<AmmAccountMeta>(), 40);

/// Taker direction, from the taker's perspective. Borsh wire encoding
/// (Long = 0, Short = 1) deliberately matches
/// [`crate::controller::position::PositionDirection`], but the CPI ABI gets
/// its own enum so it can never drift with internal refactors.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub enum Direction {
    Long,
    Short,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct QuoteArgsV0 {
    pub direction: Direction,
    /// Base size the taker wants filled.
    pub size: u64,
    /// `User`s velocity has loaded and can settle balance changes for.
    /// Quoters must not fill anyone else (velocity rejects the response
    /// otherwise). `None` = unrestricted, for off-chain quote discovery.
    pub users: Option<Vec<Pubkey>>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct QuoteResponseV0 {
    /// Levels the quoter will fill at, best price first.
    pub levels: Vec<PriceLevel>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct PriceLevel {
    pub price: u64,
    pub size: u64,
}

/// Returned via return data by `quote_v0`/`execute_v0`: where in the quoter's
/// `response_account` the borsh response was written.
#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ResponsePointerV0 {
    pub offset: u32,
    pub len: u32,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ExecuteArgsV0 {
    pub direction: Direction,
    /// Base size to fill. The quoter may partially fill; the actual fill is
    /// whatever the returned balance changes sum to.
    pub size: u64,
    /// Same contract as [`QuoteArgsV0::users`]; velocity always passes the
    /// loaded set here.
    pub users: Option<Vec<Pubkey>>,
}

#[derive(Clone, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct ExecuteResponseV0 {
    pub balance_changes: Vec<UserBalanceChange>,
    /// Sub-min remainders the quoter removed with this fill; velocity
    /// decrements the maker's open-order aggregates (that maker was just
    /// filled, so their `User` is loaded).
    pub cancelled: Vec<CancelledRemainderV0>,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct CancelledRemainderV0 {
    pub user: Pubkey,
    pub order_id: u64,
    pub base_asset_amount: u64,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Eq, Debug)]
pub struct UserBalanceChange {
    /// The User account the change applies to.
    pub user: Pubkey,
    /// Will be subtracted if direction was long (taker is taking base from
    /// this user). Will be added if direction was short (taker is adding base
    /// to this user).
    pub base_size: u64,
    /// Will be added if direction was long (taker is paying quote to this
    /// user). Will be subtracted if direction was short (taker is taking
    /// quote from this user).
    pub quote_size: u64,
}

impl QuoterV0 {
    /// CPI `quote_v0` on the quoter program and return its price levels.
    ///
    /// `account_map` is the caller's remaining-accounts index (pubkey →
    /// AccountInfo); every registered quote account must be present or the
    /// call errors — silently dropping one would misalign the CPI account
    /// list against the quoter's expectations.
    pub fn quote<'info>(
        &self,
        args: QuoteArgsV0,
        velocity_signer: &Pubkey,
        signer_nonce: u8,
        account_map: &BTreeMap<Pubkey, AccountInfo<'info>>,
    ) -> Result<Vec<PriceLevel>> {
        validate!(
            self.is_active && self.is_approved,
            ErrorCode::DefaultError,
            "quoter is not active and approved"
        )?;
        let response: QuoteResponseV0 = self.invoke_quoter(
            &self.quote_v0_discriminator,
            &self.quote_accounts,
            self.quote_accounts_count,
            &args,
            velocity_signer,
            signer_nonce,
            account_map,
        )?;
        Ok(response.levels)
    }

    /// CPI `execute_v0` on the quoter program: commit a fill and return the
    /// balance changes velocity must apply. Callers are responsible for
    /// validating the returned changes against the quoted levels (and margin)
    /// before applying them — the quoter is untrusted.
    pub fn execute<'info>(
        &self,
        args: ExecuteArgsV0,
        velocity_signer: &Pubkey,
        signer_nonce: u8,
        account_map: &BTreeMap<Pubkey, AccountInfo<'info>>,
    ) -> Result<ExecuteResponseV0> {
        validate!(
            self.is_active && self.is_approved,
            ErrorCode::DefaultError,
            "quoter is not active and approved"
        )?;
        self.invoke_quoter(
            &self.execute_v0_discriminator,
            &self.execute_accounts,
            self.execute_accounts_count,
            &args,
            velocity_signer,
            signer_nonce,
            account_map,
        )
    }

    /// Shared CPI leg: forward the registered accounts, send
    /// `discriminator ++ borsh(args)`, and decode the borsh response from the
    /// quoter's response account at the pointer returned via return data.
    fn invoke_quoter<'info, A: AnchorSerialize, R: AnchorDeserialize>(
        &self,
        discriminator: &[u8; 8],
        registered: &[AmmAccountMeta],
        count: u8,
        args: &A,
        velocity_signer: &Pubkey,
        signer_nonce: u8,
        account_map: &BTreeMap<Pubkey, AccountInfo<'info>>,
    ) -> Result<R> {
        validate!(
            (count as usize) <= registered.len(),
            ErrorCode::DefaultError,
            "prop amm accounts_count {} exceeds capacity {}",
            count,
            registered.len()
        )?;
        let registered = &registered[..count as usize];

        let mut account_metas = Vec::with_capacity(registered.len());
        let mut account_infos = Vec::with_capacity(registered.len());
        for meta in registered {
            let info = account_map.get(&meta.pubkey).ok_or_else(|| {
                msg!("prop amm account {} missing from account map", meta.pubkey);
                ErrorCode::DefaultError
            })?;
            account_metas.push(AccountMeta {
                pubkey: meta.pubkey,
                // NEVER forward outer signer privilege. Signer status
                // propagates through CPI, so a quoter handed the taker's
                // wallet as a signer could CPI to the system/token program
                // and drain it. Quoters that need to know who signed the
                // outer transaction (e.g. the `flow_authority` attestation)
                // introspect the instructions sysvar instead. The single
                // exception is velocity's own signer PDA (invoke_signed
                // below — velocity signing as itself): a registered signer
                // slot for it is how a quoter authenticates that velocity,
                // not an arbitrary caller, is invoking execute.
                is_signer: meta.pubkey == *velocity_signer,
                is_writable: meta.is_writable,
            });
            account_infos.push(info.clone());
        }
        // CPI needs the callee program's account info too.
        let program_info = account_map.get(&self.program_id).ok_or_else(|| {
            msg!("quoter program account missing from account map");
            ErrorCode::DefaultError
        })?;
        account_infos.push(program_info.clone());

        let mut data = discriminator.to_vec();
        args.serialize(&mut data).map_err(|_| {
            msg!("prop amm failed to serialize cpi args");
            ErrorCode::DefaultError
        })?;

        invoke_signed(
            &Instruction {
                program_id: self.program_id,
                accounts: account_metas,
                data,
            },
            &account_infos,
            &[&get_signer_seeds(&signer_nonce)],
        )?;

        // The payload lives in the quoter's response account; return data
        // carries only a pointer into it, so responses aren't bound by the
        // 1024-byte return-data cap. Return data is last-writer-wins within
        // the transaction; requiring the writer to be `program_id` guards
        // against reading a pointer set by a program the quoter CPI'd into.
        let (writer, pointer_data) = get_return_data().ok_or_else(|| {
            msg!("prop amm quoter set no return data");
            ErrorCode::DefaultError
        })?;
        validate!(
            writer == self.program_id,
            ErrorCode::DefaultError,
            "prop amm return data written by {} instead of quoter program",
            writer
        )?;
        let pointer =
            ResponsePointerV0::deserialize(&mut pointer_data.as_slice()).map_err(|_| {
                msg!("prop amm quoter returned undecodable response pointer");
                ErrorCode::DefaultError
            })?;

        let response_info = account_map.get(&self.response_account).ok_or_else(|| {
            msg!("prop amm response account missing from account map");
            ErrorCode::DefaultError
        })?;
        // Only the quoter program can have written an account it owns.
        validate!(
            *response_info.owner == self.program_id,
            ErrorCode::DefaultError,
            "prop amm response account not owned by quoter program"
        )?;
        let data = response_info
            .try_borrow_data()
            .map_err(|_| ErrorCode::DefaultError)?;
        let start = pointer.offset as usize;
        let end = start
            .checked_add(pointer.len as usize)
            .ok_or(ErrorCode::DefaultError)?;
        validate!(
            end <= data.len(),
            ErrorCode::DefaultError,
            "prop amm response pointer out of bounds"
        )?;

        // `deserialize` (not `try_from_slice`) so trailing bytes are
        // tolerated — lets a quoter append response fields without breaking
        // older velocity builds.
        R::deserialize(&mut &data[start..end]).map_err(|_| {
            msg!("prop amm quoter returned undecodable response");
            ErrorCode::DefaultError.into()
        })
    }
}
