use {
    crate::{
        error::ErrorCode,
        events::{VaultDepositorAction, VaultDepositorRecord, VaultDepositorV1Record},
        state::vault::Vault,
        validate, FeeUpdate, Size, VaultDepositorBase, VaultFee, VaultProtocol,
    },
    anchor_lang::prelude::*,
    static_assertions::const_assert_eq,
    std::cell::RefMut,
    velocity::math::{
        casting::Cast,
        insurance::{
            if_shares_to_vault_amount as depositor_shares_to_vault_amount,
            vault_amount_to_if_shares as vault_amount_to_depositor_shares,
        },
        safe_math::SafeMath,
    },
    velocity_macros::assert_no_slop,
};

#[assert_no_slop]
#[account(zero_copy(unsafe))]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct TokenizedVaultDepositor {
    /// The vault deposited into
    pub vault: Pubkey,
    /// The vault depositor account's pubkey. It is a pda of vault
    pub pubkey: Pubkey,
    /// The token mint for tokenized shares owned by this VaultDepositor
    pub mint: Pubkey,
    /// share of vault owned by this depositor. vault_shares / vault.total_shares is depositor's ownership of vault_equity
    vault_shares: u128,
    /// stores the vault_shares from the most recent liquidity event (redeem or issuance) before a spl token
    /// CPI is done, used to track invariants
    last_vault_shares: u128,
    /// creation ts of vault depositor
    pub last_valid_ts: i64,
    /// lifetime net deposits of vault depositor for the vault
    pub net_deposits: i64,

    /// lifetime total deposits
    pub total_deposits: u64,
    /// lifetime total withdraws
    pub total_withdraws: u64,
    /// the token amount of gain, net of the profit share taken on it, that the high-water mark
    /// already covers. `net_deposits + cumulative_profit_share_amount` is the high-water mark.
    pub cumulative_profit_share_amount: i64,
    pub profit_share_fee_paid: u64,
    /// The exponent for vault_shares decimal places at the time the tokenized vault depositor was initialized.
    /// If the vault undergoes a rebase, this TokenizedVaultDepositor can no longer issue new tokens, only redeem
    /// is possible.
    pub vault_shares_base: u32,
    /// the vault's profit share when the high-water mark was last set. Gain above the high-water
    /// mark is priced at this rate, so a later raise never prices gain earned before it.
    pub profit_share_at_basis: u32,
    /// the vault's hurdle rate when the high-water mark was last set. Gain above the high-water
    /// mark keeps this shelter, so a later cut never exposes gain earned before it.
    pub hurdle_rate_at_basis: u32,
    /// The bump for the vault pda
    pub bump: u8,
    pub padding1: [u8; 3],
    pub padding: [u64; 10],
}

impl Size for TokenizedVaultDepositor {
    const SIZE: usize = 272 + 8;
}

const_assert_eq!(
    TokenizedVaultDepositor::SIZE,
    std::mem::size_of::<TokenizedVaultDepositor>() + 8
);

impl VaultDepositorBase for TokenizedVaultDepositor {
    fn get_authority(&self) -> Pubkey {
        self.vault
    }
    fn get_pubkey(&self) -> Pubkey {
        self.pubkey
    }

    fn get_vault_shares(&self) -> u128 {
        self.vault_shares
    }
    fn set_vault_shares(&mut self, shares: u128) {
        self.vault_shares = shares;
    }

    fn get_vault_shares_base(&self) -> u32 {
        self.vault_shares_base
    }
    fn set_vault_shares_base(&mut self, base: u32) {
        self.vault_shares_base = base;
    }

    fn get_net_deposits(&self) -> i64 {
        self.net_deposits
    }
    fn set_net_deposits(&mut self, amount: i64) {
        self.net_deposits = amount;
    }

    fn get_cumulative_profit_share_amount(&self) -> i64 {
        self.cumulative_profit_share_amount
    }
    fn set_cumulative_profit_share_amount(&mut self, amount: i64) {
        self.cumulative_profit_share_amount = amount;
    }

    fn get_profit_share_fee_paid(&self) -> u64 {
        self.profit_share_fee_paid
    }
    fn set_profit_share_fee_paid(&mut self, amount: u64) {
        self.profit_share_fee_paid = amount;
    }

    fn get_profit_share_at_basis(&self) -> u32 {
        self.profit_share_at_basis
    }
    fn set_profit_share_at_basis(&mut self, profit_share: u32) {
        self.profit_share_at_basis = profit_share;
    }

    fn get_hurdle_rate_at_basis(&self) -> u32 {
        self.hurdle_rate_at_basis
    }
    fn set_hurdle_rate_at_basis(&mut self, hurdle_rate: u32) {
        self.hurdle_rate_at_basis = hurdle_rate;
    }
}

impl TokenizedVaultDepositor {
    pub fn new(
        vault: &Vault,
        pubkey: Pubkey,
        mint: Pubkey,
        vault_shares_base: u32,
        bump: u8,
        now: i64,
    ) -> Self {
        Self {
            vault: vault.pubkey,
            pubkey,
            mint,
            vault_shares: 0,
            last_vault_shares: 0,
            last_valid_ts: now,
            net_deposits: 0,
            total_deposits: 0,
            total_withdraws: 0,
            cumulative_profit_share_amount: 0,
            profit_share_fee_paid: 0,
            vault_shares_base,
            profit_share_at_basis: vault.profit_share,
            hurdle_rate_at_basis: vault.hurdle_rate,
            bump,
            padding1: [0; 3],
            padding: [0; 10],
        }
    }

    fn apply_rebase(
        &mut self,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        vault_equity: u64,
    ) -> Result<Option<u128>> {
        if let Some(rebase_divisor) =
            VaultDepositorBase::apply_rebase(self, vault, vault_protocol, vault_equity)?
        {
            self.last_vault_shares = self.get_vault_shares();
            Ok(Some(rebase_divisor))
        } else {
            Ok(None)
        }
    }

    /// Permissionless lazy rebase for the signerless
    /// `apply_rebase_tokenized_depositor` instruction.
    ///
    /// #122 — the tokenized analogue of #106. These `vault_shares` are the *shared*
    /// backing for the entire tokenized SPL supply, and the base rebase floors them
    /// by integer division. `ApplyRebaseTokenizedDepositor` carries no signer at all,
    /// so any caller could commit the lazy rebase at a moment when the divisor floors
    /// that backing to zero while the mint's supply is still live. Every holder then
    /// computes zero redeemable shares and `redeem_tokens` aborts *before* burning,
    /// so the tokens are permanently unredeemable — and unlike a paper loss this does
    /// not heal when the portfolio recovers, because the backing shares are gone.
    ///
    /// So refuse to let a third party destroy that backing. As with #106 the owner
    /// side is unaffected: the signed lifecycle actions still rebase through the
    /// unguarded path, which makes a refusal recoverable where a floored backing is
    /// not.
    ///
    /// Deliberately calls `VaultDepositorBase::apply_rebase` rather than the inherent
    /// `apply_rebase` above, to keep this path byte-for-byte what it was before the
    /// guard. Method resolution already sent the instruction to the trait method (the
    /// inherent one is private to this module), so the signerless path has never
    /// refreshed the `last_vault_shares` checkpoint that the signed paths maintain.
    /// That asymmetry looks wrong, but it is a separate concern from #122 and is not
    /// silently changed here.
    pub fn apply_rebase_public(
        &mut self,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        vault_equity: u64,
    ) -> Result<Option<u128>> {
        let vault_shares_before = self.get_vault_shares();

        let rebase_divisor =
            VaultDepositorBase::apply_rebase(self, vault, vault_protocol, vault_equity)?;

        validate!(
            !(vault_shares_before > 0 && self.get_vault_shares() == 0),
            ErrorCode::InvalidVaultRebase,
            "public rebase would floor the tokenized depositor's backing shares to zero \
             and strand the live token supply; rebase via a signed action instead"
        )?;

        Ok(rebase_divisor)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn tokenize_shares(
        self: &mut TokenizedVaultDepositor,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<VaultProtocol>>,
        fee_update: &mut Option<AccountLoader<FeeUpdate>>,
        mint_supply: u64,
        vault_equity: u64,
        shares_transferred: u128,
        now: i64,
        deposit_oracle_price: i64,
    ) -> Result<u64> {
        // #107: apply the fee before the rebase check so a fee-induced vault rebase is caught by
        // the same guard (tokenization is disallowed once a rebase occurs) rather than aborting
        // the later base-checked ops with InvalidVaultRebase.
        let VaultFee {
            management_fee_payment,
            management_fee_shares,
            protocol_fee_payment,
            protocol_fee_shares,
        } = vault.apply_fee(vault_protocol, fee_update, vault_equity, now)?;
        let rebase_divisor = self.apply_rebase(vault, vault_protocol, vault_equity)?;
        if rebase_divisor.is_some() {
            return Err(ErrorCode::InvalidVaultRebase.into());
        }
        let (manager_profit_share, protocol_profit_share) =
            self.apply_profit_share(vault_equity, vault, vault_protocol)?;

        // #140: this account holds ONE cost basis for every holder of `mint`. The profit-share fee
        // comes out of the pool's own shares, so it dilutes every token equally, whoever accrued the
        // loss.
        //
        // Both `transfer_shares` legs move basis by the CURRENT VALUE of the shares moved. The pooled
        // loss shelter (basis - value) is therefore invariant to supply, while each token consumes a
        // pro-rata part of it. Minting into an under-water pool hands the newcomer part of the
        // incumbents' shelter.
        //
        // A single pooled basis can be fair only when basis == value at the supply change.
        // `apply_profit_share` above already forces that whenever value > basis. So rejecting the
        // value < basis case is sufficient, and is the tightest condition available without per-holder
        // state. Per-holder state cannot work here anyway: the fee comes out of shared pool shares, and
        // this is a classic SPL mint whose transfers the program never sees.
        //
        // The test runs POST-transfer on purpose. `tokenize_shares` has already moved the newcomer's
        // shares and basis in, but a mint raises basis and value by the same amount. So
        // `value + v >= basis + v` is the same test as `value >= basis`, and `withdraw_value` needs no
        // plumbing out of `transfer_shares`.
        //
        // Use `>=`, not `==`. #104 can defer a sub-share fee and leave value > basis, and
        // `WithdrawUnit::Token` can introduce a difference of 1.
        let pool_value = depositor_shares_to_vault_amount(
            self.get_vault_shares().cast()?,
            vault.total_shares.cast()?,
            vault_equity.cast()?,
        )?;
        let cost_basis = self
            .get_net_deposits()
            .safe_add(self.get_cumulative_profit_share_amount())?;

        validate!(
            pool_value.cast::<i64>()? >= cost_basis,
            ErrorCode::InvalidTokenization,
            "cannot tokenize into an under-water tokenized depositor: pool value {} < pooled cost basis {}",
            pool_value,
            cost_basis
        )?;

        let vault_shares_before = self.checked_vault_shares(vault)?;
        let total_vault_shares_before = vault.total_shares;
        let user_vault_shares_before = vault.user_shares;
        let protocol_shares_before = vault.get_protocol_shares(vault_protocol);

        let new_last_vault_shares = self.last_vault_shares.safe_add(shares_transferred)?;

        validate!(
            new_last_vault_shares == vault_shares_before,
            ErrorCode::InvalidVaultSharesDetected,
            "TokenizedVaultDepositor: last_vault_shares + shares_transferred != vault_shares, {} != {}",
            new_last_vault_shares,
            vault_shares_before
        )?;

        let tokens_to_mint = vault_amount_to_depositor_shares(
            shares_transferred.cast()?,
            mint_supply.cast()?,
            self.last_vault_shares.cast()?,
        )?;

        msg!(
            "shares_transferred: {}, tokenized_vd.last_vault_shares: {}, token_supply_before: {}, tokens_to_mint: {}",
            shares_transferred,
            self.last_vault_shares,
            mint_supply,
            tokens_to_mint
        );

        self.last_vault_shares = self.checked_vault_shares(vault)?;

        match vault_protocol {
            None => {
                emit!(VaultDepositorRecord {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: vault.pubkey,
                    action: VaultDepositorAction::TokenizeShares,
                    amount: shares_transferred.cast()?,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after: self.last_vault_shares,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    profit_share: manager_profit_share
                        .safe_add(protocol_profit_share)?
                        .cast()?,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    deposit_oracle_price,
                });
            }
            Some(_) => {
                emit!(VaultDepositorV1Record {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: vault.pubkey,
                    action: VaultDepositorAction::TokenizeShares,
                    amount: shares_transferred.cast()?,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after: self.last_vault_shares,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    protocol_profit_share,
                    protocol_fee: protocol_fee_payment,
                    protocol_fee_shares,
                    manager_profit_share,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    protocol_shares_before,
                    protocol_shares_after: vault.get_protocol_shares(vault_protocol),
                    deposit_oracle_price,
                });
            }
        }

        Ok(tokens_to_mint.cast()?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn redeem_tokens<'a>(
        self: &mut TokenizedVaultDepositor,
        vault: &mut Vault,
        vault_protocol: &mut Option<RefMut<'a, VaultProtocol>>,
        fee_update: &mut Option<AccountLoader<FeeUpdate>>,
        mint_supply: u64,
        vault_equity: u64,
        tokens_to_burn: u64,
        now: i64,
        deposit_oracle_price: i64,
    ) -> Result<(u64, Option<RefMut<'a, VaultProtocol>>)> {
        self.apply_rebase(vault, vault_protocol, vault_equity)?;

        let VaultFee {
            management_fee_payment,
            management_fee_shares,
            protocol_fee_payment,
            protocol_fee_shares,
        } = vault.apply_fee(vault_protocol, fee_update, vault_equity, now)?;
        // #107: re-sync in case apply_fee induced a vault rebase, so the base-checked
        // apply_profit_share below does not abort with InvalidVaultRebase.
        self.apply_rebase(vault, vault_protocol, vault_equity)?;
        let (manager_profit_share, protocol_profit_share) =
            self.apply_profit_share(vault_equity, vault, vault_protocol)?;

        let vault_shares_before = self.checked_vault_shares(vault)?;
        let total_vault_shares_before = vault.total_shares;
        let user_vault_shares_before = vault.user_shares;
        let protocol_shares_before = vault.get_protocol_shares(vault_protocol);

        self.last_vault_shares = self.checked_vault_shares(vault)?;

        let shares_to_redeem = depositor_shares_to_vault_amount(
            tokens_to_burn.cast()?,
            mint_supply.cast()?,
            self.last_vault_shares.cast()?,
        )?;

        msg!(
            "tokens_to_burn: {}, tokenized_vd.vault_shares: {}, token_supply_before: {}, shares_to_redeem: {}",
            tokens_to_burn,
            self.last_vault_shares,
            mint_supply,
            shares_to_redeem
        );

        match vault_protocol {
            None => {
                emit!(VaultDepositorRecord {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: vault.pubkey,
                    action: VaultDepositorAction::RedeemTokens,
                    amount: tokens_to_burn,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after: self.last_vault_shares,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    profit_share: manager_profit_share
                        .safe_add(protocol_profit_share)?
                        .cast()?,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    deposit_oracle_price
                });
            }
            Some(_) => {
                emit!(VaultDepositorV1Record {
                    ts: now,
                    vault: vault.pubkey,
                    depositor_authority: vault.pubkey,
                    action: VaultDepositorAction::RedeemTokens,
                    amount: tokens_to_burn,
                    spot_market_index: vault.spot_market_index,
                    vault_equity_before: vault_equity,
                    vault_shares_before,
                    user_vault_shares_before,
                    total_vault_shares_before,
                    vault_shares_after: self.vault_shares,
                    total_vault_shares_after: vault.total_shares,
                    user_vault_shares_after: vault.user_shares,
                    protocol_profit_share,
                    protocol_fee: protocol_fee_payment,
                    protocol_fee_shares,
                    manager_profit_share,
                    management_fee: management_fee_payment,
                    management_fee_shares,
                    protocol_shares_before,
                    protocol_shares_after: vault.get_protocol_shares(vault_protocol),
                    deposit_oracle_price
                });
            }
        }

        Ok((shares_to_redeem, vault_protocol.take()))
    }

    /// #105: re-checkpoint `last_vault_shares` to the current `vault_shares`. The redeem
    /// instruction moves shares out of this tokenized depositor via `transfer_shares` *after*
    /// [`TokenizedVaultDepositor::redeem_tokens`] returns; without lowering the checkpoint to the
    /// post-transfer balance, the stale (pre-transfer) checkpoint permanently breaks future
    /// `tokenize_shares` (the `last_vault_shares + shares_transferred == vault_shares` invariant
    /// can no longer hold). Callers must invoke this after the redeem share transfer completes.
    pub fn checkpoint_vault_shares(&mut self) {
        self.last_vault_shares = self.vault_shares;
    }

    /// Clear the pooled cost basis once the pool holds no shares and no tokens (#140).
    ///
    /// Both `transfer_shares` legs move basis by the current value of the shares moved. A full
    /// redemption therefore leaves `net_deposits + cumulative_profit_share_amount` behind with no
    /// holders behind it. The next tokenizer inherits it, and it cuts both ways:
    ///
    /// - orphaned ABOVE value, when the pool was under water as it drained, is a free loss shelter.
    ///   The next tokenizer pays no profit share on a recovery whose drawdown it never suffered. No
    ///   holder is harmed, so the manager's fee revenue takes the whole loss.
    /// - orphaned BELOW value is the mirror. It is reachable when `hurdle_rate > 0` leaves a
    ///   sub-hurdle profit without advancing the mark, or when #104 defers a sub-share fee and rolls
    ///   it back. The next honest tokenizer then owes profit share on gains it never made.
    ///
    /// A pool with no shares and no tokens has no holders, so the reset is invisible to everyone.
    /// `profit_share_fee_paid`, `total_deposits` and `total_withdraws` are lifetime analytics and stay.
    ///
    /// The under-water gate in [`Self::tokenize_shares`] already covers the security case, because an
    /// empty pool with a positive basis fails it. This is kept because it is free, and because it is
    /// the only fix for the below-value direction, which is an honest-user bug.
    pub fn reset_orphaned_cost_basis(&mut self) {
        self.net_deposits = 0;
        self.cumulative_profit_share_amount = 0;
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{TokenizedVaultDepositor, Vault, VaultDepositorBase},
        anchor_lang::prelude::Pubkey,
        velocity::math::{constants::PERCENTAGE_PRECISION, safe_math::SafeMath},
    };

    #[test]
    fn test_tokenize_shares() {
        let now = 1337;
        let vault = &mut Vault::default();
        let mut tvd =
            TokenizedVaultDepositor::new(vault, Pubkey::default(), Pubkey::default(), 0, 0, now);
        let mut shares_transferred = 100_000;
        tvd.vault_shares = tvd.last_vault_shares + shares_transferred;

        assert_eq!(tvd.last_vault_shares, 0);

        let mut total_supply = 0;
        let vault_equity = 1_000_000;
        let tokens_issued_1 = tvd
            .tokenize_shares(
                vault,
                &mut None,
                &mut None,
                total_supply,
                vault_equity,
                shares_transferred,
                now,
                0,
            )
            .unwrap();

        // first tokenization will issue same amount of tokens as shares
        assert_eq!(tokens_issued_1, shares_transferred as u64);
        assert_eq!(tvd.last_vault_shares, tvd.vault_shares);

        // emulate minting tokens
        total_supply += tokens_issued_1;

        // second tokenization is double the shares of first issuance``
        shares_transferred *= 2;
        tvd.vault_shares = tvd.last_vault_shares + shares_transferred;

        let tokens_issued_2 = tvd
            .tokenize_shares(
                vault,
                &mut None,
                &mut None,
                total_supply,
                vault_equity,
                shares_transferred,
                now,
                0,
            )
            .unwrap();

        // first tokenization will issue same amount of tokens as shares
        assert_eq!(tokens_issued_2, tokens_issued_1 * 2);
        assert_eq!(tvd.last_vault_shares, tvd.vault_shares);
        assert_eq!(
            tvd.vault_shares,
            (tokens_issued_1 + tokens_issued_2) as u128
        );
    }

    #[test]
    fn test_redeem_tokens() {
        let now = 1337;
        let vault = &mut Vault::default();
        let mut tvd =
            TokenizedVaultDepositor::new(vault, Pubkey::default(), Pubkey::default(), 0, 0, now);
        let shares_transferred = 500_000;
        tvd.vault_shares = shares_transferred;
        tvd.last_vault_shares = tvd.vault_shares;

        assert_eq!(tvd.last_vault_shares, shares_transferred);

        let total_supply = shares_transferred;
        let vault_equity = 1_000_000;

        // redeem 50% of tokens
        let tokens_to_burn = total_supply / 2;
        let shares_to_transfer = tvd
            .redeem_tokens(
                vault,
                &mut None,
                &mut None,
                total_supply as u64,
                vault_equity,
                tokens_to_burn as u64,
                now,
                0,
            )
            .expect("redeem_tokens");
        assert_eq!(shares_to_transfer.0, tokens_to_burn as u64);
        assert_eq!(tvd.last_vault_shares, tvd.vault_shares);
    }

    #[test]
    fn test_tokenize_shares_with_rebase() {
        let mut now = 1337;
        let vault = &mut Vault::default();
        let mut tvd =
            TokenizedVaultDepositor::new(vault, Pubkey::default(), Pubkey::default(), 0, 0, now);
        let shares_transferred = 100_000;
        tvd.vault_shares = tvd.last_vault_shares + shares_transferred;

        assert_eq!(tvd.last_vault_shares, 0);

        let mut total_supply = 0;
        let mut vault_equity = 1_000_000;
        let tokens_issued_1 = tvd
            .tokenize_shares(
                vault,
                &mut None,
                &mut None,
                total_supply,
                vault_equity,
                shares_transferred,
                now,
                0,
            )
            .unwrap();

        // first tokenization will issue same amount of tokens as shares
        assert_eq!(tokens_issued_1, shares_transferred as u64);
        assert_eq!(tvd.last_vault_shares, tvd.vault_shares);

        // emulate minting tokens
        total_supply += tokens_issued_1;

        // second tokenization happens after vault down 99.9%
        vault_equity /= 1000;
        now += 100;

        tvd.vault_shares = tvd.last_vault_shares + shares_transferred;

        // will trigger rebase
        let tokens_issued_2 = tvd.tokenize_shares(
            vault,
            &mut None,
            &mut None,
            total_supply,
            vault_equity,
            shares_transferred,
            now,
            0,
        );

        assert!(
            tokens_issued_2.is_err(),
            "disallow tokenize_shares on rebase"
        );
    }

    #[test]
    fn test_tokenize_shares_with_profit_share() {
        let now = 1337;
        let vault = &mut Vault::default();
        let profit_share_pct = 10u64;
        vault.profit_share = PERCENTAGE_PRECISION
            .safe_div(profit_share_pct as u128)
            .unwrap() as u32;
        let mut tvd =
            TokenizedVaultDepositor::new(vault, Pubkey::default(), Pubkey::default(), 0, 0, now);

        let total_supply = 0;
        let vault_equity = 1_000_000u64;
        let shares_transferred = 100_000;

        vault.user_shares = shares_transferred;
        vault.total_shares = shares_transferred;
        tvd.vault_shares = tvd.last_vault_shares + shares_transferred;
        tvd.net_deposits = vault_equity as i64;

        assert_eq!(tvd.last_vault_shares, 0);

        let tokens_issued_1 = tvd
            .tokenize_shares(
                vault,
                &mut None,
                &mut None,
                total_supply,
                vault_equity,
                shares_transferred,
                now,
                0,
            )
            .unwrap();

        // first tokenization will issue same amount of tokens as shares
        assert_eq!(tokens_issued_1, shares_transferred as u64);
        assert_eq!(tvd.last_vault_shares, tvd.vault_shares);

        let profit = vault_equity * profit_share_pct * 2 / 100;
        println!("profit: {}", profit);

        let tvd_shares_before = tvd.get_vault_shares();
        let (manager_profit_share, protocol_profit_share) = tvd
            .apply_profit_share(vault_equity + profit, vault, &mut None)
            .unwrap();
        let tvd_shares_after = tvd.get_vault_shares();

        println!(
            "tvd_shares_before: {}, tvd_shares_after: {}",
            tvd_shares_before, tvd_shares_after
        );

        assert_eq!(
            manager_profit_share + protocol_profit_share,
            profit * profit_share_pct / 100
        );
        assert!(
            tvd_shares_after < tvd_shares_before,
            "tvd shares should decrease after profit share"
        );
    }

    // OtterSec #105: after a redeem moves shares out, checkpoint_vault_shares must lower the
    // last_vault_shares checkpoint so a later tokenize still satisfies
    // last_vault_shares + shares_transferred == vault_shares.
    #[test]
    fn test_redeem_then_tokenize_after_checkpoint() {
        let now = 1337;
        let vault = &mut Vault::default();
        let mut tvd =
            TokenizedVaultDepositor::new(vault, Pubkey::default(), Pubkey::default(), 0, 0, now);
        let shares_transferred = 500_000u128;
        tvd.vault_shares = shares_transferred;
        tvd.last_vault_shares = shares_transferred;

        let total_supply = shares_transferred as u64;
        // Share price of exactly 1, so basis and value move by the same figures below.
        let vault_equity = 500_000;
        vault.total_shares = shares_transferred;

        // The pool's cost basis is maintained by `transfer_shares` at the instruction layer, which
        // this unit test bypasses along with the share movement itself. It has to be emulated too:
        // left at zero, the first `apply_profit_share` books the whole pool as phantom profit and
        // parks the high-water mark at full value, so any later outflow reads as under water and the
        // #140 gate (correctly) refuses. Start at basis == value.
        tvd.net_deposits = vault_equity as i64;

        // redeem 50%
        let tokens_to_burn = total_supply / 2;
        let (shares_to_redeem, _) = tvd
            .redeem_tokens(
                vault,
                &mut None,
                &mut None,
                total_supply,
                vault_equity,
                tokens_to_burn,
                now,
                0,
            )
            .expect("redeem_tokens");

        // simulate the instruction moving the shares out and re-checkpointing (#105)
        tvd.vault_shares -= shares_to_redeem as u128;
        tvd.net_deposits -= shares_to_redeem as i64;
        tvd.checkpoint_vault_shares();
        assert_eq!(tvd.last_vault_shares, tvd.vault_shares);

        // a new tokenization into the same tokenized depositor must now succeed
        let new_shares = 100_000u128;
        tvd.vault_shares += new_shares;
        tvd.net_deposits += new_shares as i64;
        let res = tvd.tokenize_shares(
            vault,
            &mut None,
            &mut None,
            total_supply - tokens_to_burn,
            vault_equity,
            new_shares,
            now,
            0,
        );
        assert!(
            res.is_ok(),
            "tokenize after redeem must succeed once the checkpoint is refreshed: {:?}",
            res.err()
        );
        assert_eq!(tvd.last_vault_shares, tvd.vault_shares);
    }

    // OtterSec #100 (contract): redeem_tokens returns the VaultProtocol provider (via take()) so
    // the instruction can keep it alive across the before/after conservation snapshots instead of
    // discarding it and having get_manager_shares switch to counting protocol shares as manager.
    #[test]
    fn test_redeem_tokens_returns_provider() {
        use {crate::VaultProtocol, std::cell::RefCell};

        let now = 1337;
        let vault = &mut Vault::default();
        let vp = RefCell::new(VaultProtocol::default());
        let mut tvd =
            TokenizedVaultDepositor::new(vault, Pubkey::default(), Pubkey::default(), 0, 0, now);
        let shares = 500_000u128;
        tvd.vault_shares = shares;
        tvd.last_vault_shares = shares;
        vault.total_shares = shares;
        vault.user_shares = shares;

        let (_, returned_vp) = tvd
            .redeem_tokens(
                vault,
                &mut Some(vp.borrow_mut()),
                &mut None,
                shares as u64,
                1_000_000,
                (shares / 2) as u64,
                now,
                0,
            )
            .expect("redeem_tokens");
        assert!(
            returned_vp.is_some(),
            "redeem_tokens must return the provider for the instruction to keep alive"
        );
    }

    /// #140: tokenizing into an under-water pool is refused.
    ///
    /// The pool holds one cost basis for all holders and the profit-share fee comes out of pool
    /// shares, so it dilutes every token equally. A newcomer minting while basis > value buys a slice
    /// of the incumbents' loss shelter: on recovery the fee is computed against the *pooled* basis,
    /// so the incumbents pay profit share on a gain that is partly the newcomer's, and the newcomer
    /// pays less than they would standing alone. It is a pure holder-to-holder transfer — the manager
    /// collects the same either way — which is why no basis-rewriting rule can fix it and the mint
    /// itself has to be refused.
    #[test]
    fn under_water_pool_refuses_tokenization() {
        let now = 1337;
        let vault = &mut Vault::default();
        let mut tvd =
            TokenizedVaultDepositor::new(vault, Pubkey::default(), Pubkey::default(), 0, 0, now);

        // 1000 shares tokenized when the share price was 1.
        let existing_shares = 1_000u128;
        tvd.vault_shares = existing_shares;
        tvd.last_vault_shares = existing_shares;
        tvd.net_deposits = 1_000;
        vault.total_shares = existing_shares;

        // The vault then falls 40%: the pool is worth 600 against a basis of 1000.
        let vault_equity_after_drawdown = 600u64;

        // A newcomer's shares have already been moved in by `transfer_shares` when the state fn
        // runs, so model that: +1000 shares carrying +600 of basis at today's price.
        let new_shares = 1_000u128;
        tvd.vault_shares += new_shares;
        tvd.net_deposits += 600;
        vault.total_shares += new_shares;

        let res = tvd.tokenize_shares(
            vault,
            &mut None,
            &mut None,
            existing_shares as u64,
            vault_equity_after_drawdown * 2,
            new_shares,
            now,
            0,
        );

        assert!(
            res.is_err(),
            "tokenizing into an under-water pool must be refused, got {:?}",
            res.ok()
        );
    }

    /// #140: a pool at or above its basis still accepts tokenizations.
    ///
    /// The companion to the test above — the gate must not be a blanket ban. `apply_profit_share`
    /// already forces basis == value whenever value > basis, so the healthy case has to keep working
    /// or tokenization would be dead entirely.
    #[test]
    fn healthy_pool_still_accepts_tokenization() {
        let now = 1337;
        let vault = &mut Vault::default();
        let mut tvd =
            TokenizedVaultDepositor::new(vault, Pubkey::default(), Pubkey::default(), 0, 0, now);

        let existing_shares = 1_000u128;
        tvd.vault_shares = existing_shares;
        tvd.last_vault_shares = existing_shares;
        tvd.net_deposits = 1_000;
        vault.total_shares = existing_shares;

        // Share price of exactly 1: value == basis, the boundary the `>=` admits.
        let new_shares = 1_000u128;
        tvd.vault_shares += new_shares;
        tvd.net_deposits += 1_000;
        vault.total_shares += new_shares;

        let res = tvd.tokenize_shares(
            vault,
            &mut None,
            &mut None,
            existing_shares as u64,
            2_000,
            new_shares,
            now,
            0,
        );

        assert!(
            res.is_ok(),
            "a pool at its cost basis must still accept tokenization: {:?}",
            res.err()
        );
    }

    /// #140: draining a pool must not leave its cost basis behind for the next tokenizer.
    ///
    /// This is the sharpest form of the finding: it needs no victim, no timing race and no
    /// coordination. Redeem every token, and `transfer_shares` has reduced `net_deposits` by only the
    /// *current value* of the shares that left — so a pool drained while under water keeps the whole
    /// shelter with nobody behind it. Whoever tokenizes next inherits it and pays zero profit share
    /// on the recovery. The loss falls entirely on the manager's fee revenue.
    ///
    /// It cuts the other way too: orphaned *below* value, the next honest tokenizer immediately owes
    /// profit share on gains they never made.
    #[test]
    fn emptied_pool_drops_its_orphaned_cost_basis() {
        let now = 1337;
        let vault = Vault::default();
        let mut tvd =
            TokenizedVaultDepositor::new(&vault, Pubkey::default(), Pubkey::default(), 0, 0, now);

        // A pool drained at 50% down: the departing holder correctly ate the loss in their own
        // VaultDepositor, and `transfer_shares` left basis 500 against value 0.
        tvd.vault_shares = 0;
        tvd.last_vault_shares = 0;
        tvd.net_deposits = 500;
        tvd.cumulative_profit_share_amount = 250;

        tvd.reset_orphaned_cost_basis();

        assert_eq!(tvd.get_net_deposits(), 0);
        assert_eq!(tvd.get_cumulative_profit_share_amount(), 0);
    }

    /// OtterSec #122: the signerless `apply_rebase_tokenized_depositor` must not floor
    /// the shared backing for a live token supply to zero. Mirrors the #106 guard on
    /// `VaultDepositor::apply_rebase_public`.
    #[test]
    fn test_tokenized_apply_rebase_public_rejects_flooring_backing_to_zero() {
        let now = 1000;

        // Tiny backing: the divisor floors it to zero, which would leave every token
        // holder computing zero redeemable shares with `redeem_tokens` aborting before
        // it burns. Must be rejected.
        {
            let mut vault = Vault::default();
            let mut vp = None;
            vault.total_shares = 200_000_000;
            vault.user_shares = 200_000_000;
            let tvd = &mut TokenizedVaultDepositor::new(
                &vault,
                Pubkey::default(),
                Pubkey::default(),
                0,
                0,
                now,
            );
            tvd.set_vault_shares(10);
            let vault_equity: u64 = 2; // divisor 1e7 -> 10 shares floor to 0
            let res = tvd.apply_rebase_public(&mut vault, &mut vp, vault_equity);
            assert!(
                res.is_err(),
                "public rebase must reject flooring live token backing to zero"
            );
            // Confirmed the unguarded path really does floor it, i.e. the guard is
            // what prevents this rather than the arithmetic being harmless.
            let mut vault2 = Vault::default();
            let mut vp2 = None;
            vault2.total_shares = 200_000_000;
            vault2.user_shares = 200_000_000;
            let tvd2 = &mut TokenizedVaultDepositor::new(
                &vault,
                Pubkey::default(),
                Pubkey::default(),
                0,
                0,
                now,
            );
            tvd2.set_vault_shares(10);
            VaultDepositorBase::apply_rebase(tvd2, &mut vault2, &mut vp2, vault_equity).unwrap();
            assert_eq!(
                tvd2.get_vault_shares(),
                0,
                "fixture must actually floor to zero to reproduce #122"
            );
        }

        // Backing large enough to survive the divisor: unaffected.
        {
            let mut vault = Vault::default();
            let mut vp = None;
            vault.total_shares = 200_000_000;
            vault.user_shares = 200_000_000;
            let tvd = &mut TokenizedVaultDepositor::new(
                &vault,
                Pubkey::default(),
                Pubkey::default(),
                0,
                0,
                now,
            );
            tvd.set_vault_shares(100_000_000);
            let vault_equity: u64 = 2; // divisor 1e7 -> 1e8 shares -> 10 (nonzero)
            let res = tvd.apply_rebase_public(&mut vault, &mut vp, vault_equity);
            assert!(res.is_ok(), "public rebase should succeed: {:?}", res.err());
            assert_eq!(tvd.get_vault_shares_base(), vault.shares_base);
            assert!(tvd.get_vault_shares() > 0);
        }
    }
}
