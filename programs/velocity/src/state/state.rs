// Anchor's IDL source parser expands the account-field aliases and needs these
// names in scope even though the runtime Rust compiler does not. Do not remove
// this apparently-unused import without regenerating and checking the IDL.
#[allow(unused_imports)]
use crate::math::time::{StoredSlotDuration, STORED_UNIT_MS};
use {
    crate::{
        error::VelocityResult,
        math::{
            casting::Cast,
            constants::{
                FEE_DENOMINATOR, FEE_PERCENTAGE_DENOMINATOR, LAMPORTS_PER_SOL_U64,
                PERCENTAGE_PRECISION_U64, TWENTY_FOUR_HOUR,
            },
            safe_math::SafeMath,
            safe_unwrap::SafeUnwrap,
            time::{
                legacy_slot_duration_i64, legacy_slot_duration_i64_to_millis,
                legacy_slot_duration_u8, legacy_slot_duration_u8_to_millis, LegacySlotDurationI64,
                LegacySlotDurationU8, Millis, SlotClock, SlotDuration,
            },
        },
        state::traits::Size,
    },
    anchor_lang::prelude::*,
    enumflags2::BitFlags,
};

#[cfg(test)]
mod tests;

#[account(zero_copy(unsafe))]
#[repr(C)]
#[derive(Debug)]
pub struct State {
    /// Root authority. Set at `initialize`; only this key can rotate `warm_admin`
    /// and `pause_admin`. Expected to sit behind a (small) timelocked multisig.
    pub cold_admin: Pubkey,
    /// Operational authority (e.g. multisig+timelock). Can rotate the 10 hot keys
    /// below. `Pubkey::default()` means unset — only `cold_admin` can act in that case.
    pub warm_admin: Pubkey,
    /// Emergency pause authority. No onchain timelock — intended to live behind a
    /// fast-acting multisig that can flip pause flags without delay. May only *add*
    /// pause bits (never clear them); cold/warm retain full pause + unpause power.
    /// `Pubkey::default()` means unassigned (only cold/warm can pause).
    pub pause_admin: Pubkey,
    /// Purpose-specific bot keys. `Pubkey::default()` means the role is unassigned
    /// and only warm/cold can call handlers gated on that role.
    pub hot_amm_crank: Pubkey,
    pub hot_lp_cache: Pubkey,
    pub hot_lp_swap: Pubkey,
    pub hot_lp_settle: Pubkey,
    pub hot_feature_flag: Pubkey,
    pub hot_fuel: Pubkey,
    pub hot_user_flag: Pubkey,
    pub hot_vault_deposit: Pubkey,
    pub hot_mm_oracle_crank: Pubkey,
    /// Bot authority for the low-CU native AMM spread-adjustment crank.
    pub hot_amm_spread_adjust: Pubkey,

    pub whitelist_mint: Pubkey,
    pub discount_mint: Pubkey,
    pub signer: Pubkey,
    pub srm_vault: Pubkey,
    pub perp_fee_structure: FeeStructure,
    pub spot_fee_structure: FeeStructure,
    pub oracle_guard_rails: OracleGuardRails,
    pub number_of_authorities: u64,
    pub number_of_sub_accounts: u64,
    pub liquidation_margin_buffer_ratio: u32,
    pub settlement_duration: u16,
    pub number_of_markets: u16,
    pub number_of_spot_markets: u16,
    pub signer_nonce: u8,
    /// Compact wall-clock duration encoded in historical 400ms slot quanta.
    pub min_perp_auction_duration: LegacySlotDurationU8,
    /// Default time-in-force for market orders, in seconds. `Order.max_ts` is a
    /// unix timestamp, so this never converts through the slot length and stays
    /// a raw integer. It currently has no onchain reader.
    pub default_market_order_time_in_force: u8,
    /// An actual slot-count setting, not a wall-clock duration. It currently has
    /// no onchain reader (spot DLOB trading is disabled), so it intentionally
    /// remains raw rather than using `StoredSlotDuration`.
    pub default_spot_auction_duration: u8,
    pub exchange_status: u8,
    /// Compact wall-clock duration encoded in historical 400ms slot quanta.
    pub liquidation_duration: LegacySlotDurationU8,
    pub initial_pct_to_liquidate: u16,
    pub max_number_of_sub_accounts: u16,
    pub max_initialize_user_fee: u16,
    pub feature_bit_flags: u8,
    pub lp_pool_feature_bit_flags: u8,
    /// Bitmask of `SolvencyStatus` flags. Gates internal solvency-repair flows
    /// (bankruptcy / pnl-deficit resolution) independently of `WithdrawPaused`,
    /// so user withdrawals can be halted while repair keeps running, or repair
    /// can be frozen on its own when an oracle is suspect. `0` = repair allowed.
    pub solvency_status: u8,
    /// Treasury that PERP protocol fees (quote-denominated) may be withdrawn
    /// to. Settable only by `cold_admin`. `withdraw_protocol_fees_perp` pays
    /// this key's associated token account (recipient-locked).
    /// `Pubkey::default()` (unset) makes perp withdrawals inert.
    pub protocol_fee_recipient_perp: Pubkey,
    /// Treasury that SPOT protocol fees (each market's own token: lending
    /// carveouts + spot-liquidation cuts) may be withdrawn to. Settable only
    /// by `cold_admin`. `withdraw_protocol_fees_spot` pays this key's
    /// associated token account for the market's mint (recipient-locked).
    /// `Pubkey::default()` (unset) makes spot withdrawals inert.
    pub protocol_fee_recipient_spot: Pubkey,
    /// Hot key authorized for the `FeeWithdraw` role (triggers protocol-fee
    /// withdrawals to the configured recipients).
    pub hot_fee_withdraw: Pubkey,
    /// Hot key authorized for the `AccountExtension` role (grows zero-copy
    /// accounts to the deployed program's size after a struct-extending
    /// upgrade).
    pub hot_account_extension: Pubkey,
    /// Promotional fee-tier floor applied to every account: the effective
    /// perp fee tier is `max(volume tier, promo_fee_tier)` (clamped to the
    /// configured tier count), so nobody is downgraded by it. 0 = no-op
    /// (disabled), also what pre-upgrade accounts read from former padding.
    /// Reset to 0 and every account is back on its volume tier at its next
    /// fill; no per-user state.
    pub promo_fee_tier: u8,
    /// Legacy current slot duration field in milliseconds, kept coherent by
    /// the permissionless sync as the IBRL feature gates activate
    /// (400 -> 350 -> 300 -> 250 -> 200). `0` means unset (what pre upgrade
    /// accounts read out of former padding) and is interpreted as the 400ms
    /// baseline. Never read this field directly, use [`State::slot_clock`] /
    /// [`State::slot_duration`]; once any `slot_duration_transition_slots`
    /// entry is set the archive is authoritative over this field.
    pub slot_duration_ms: u16,
    /// Legacy staged next slot duration in ms, kept coherent by the
    /// permissionless sync for older readers. `0` means nothing is staged. Once
    /// `slot_duration_effective_slot` is reached, the legacy resolution returns
    /// this value instead of `slot_duration_ms`. Superseded by the transition
    /// archive.
    pub pending_slot_duration_ms: u16,
    /// Explicit padding so `slot_duration_effective_slot` (u64) lands on its
    /// 8-byte alignment with no *implicit* padding (see the alignment invariant).
    pub slot_duration_pad: [u8; 2],
    /// Slot at which `pending_slot_duration_ms` takes effect: the first slot of
    /// the epoch after the target gate's activation epoch, derived from the
    /// `EpochSchedule` sysvar at sync time. `0` when nothing is staged.
    pub slot_duration_effective_slot: u64,
    /// First slot of each post baseline IBRL regime, ordered as
    /// `[350ms, 300ms, 250ms, 200ms]`. Zero means that transition has not been
    /// synchronized yet. These anchors let elapsed time math integrate an
    /// interval piecewise instead of multiplying its whole slot delta by the
    /// duration at one endpoint.
    pub slot_duration_transition_slots: [u64; 4],
    /// Active-management authority for scoped vAMM quoting controls carried by
    /// `HotAdminUpdatePerpMarket`. This may be a multisig PDA; timelock policy
    /// lives in that multisig. Added from former padding so existing fields,
    /// including `hot_amm_spread_adjust`, retain their offsets.
    pub hot_vamm_quote_management: Pubkey,
    /// 168 = the former 244 byte padding minus the 12 staging bytes, the 32 bytes
    /// used by `slot_duration_transition_slots`, and the 32-byte quote management
    /// authority.
    /// (`pending_slot_duration_ms` 2 + `slot_duration_pad` 2 + the 8-byte
    /// `slot_duration_effective_slot`). The padding still absorbs the 8 bytes that
    /// were previously *implicit* trailing padding on x86_64 (State contains a
    /// u128, align 16 on the host but 8 on SBF; explicit padding keeps
    /// `size_of::<State>()` 1744 on both targets, per the alignment invariant in
    /// docs/alignment-and-native-offsets.md).
    pub padding: [u8; 168],
}

/// Purpose-specific hot role keys held on `State`. Each variant maps to one of the
/// `hot_*` pubkey fields and is used by `State::require_hot` / `hot_key`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Eq, PartialEq)]
pub enum HotRole {
    AmmCrank,
    LpCache,
    LpSwap,
    LpSettle,
    FeatureFlag,
    Fuel,
    UserFlag,
    VaultDeposit,
    MmOracleCrank,
    AmmSpreadAdjust,
    FeeWithdraw,
    AccountExtension,
    /// vAMM active management (spread/JIT/curve and related quoting controls).
    /// Appended to preserve every existing role's serialized ordinal.
    VammQuoteManagement,
}

#[derive(BitFlags, Clone, Copy, PartialEq, Debug, Eq)]
pub enum ExchangeStatus {
    // Active = 0b00000000
    DepositPaused = 0b00000001,
    WithdrawPaused = 0b00000010,
    AmmPaused = 0b00000100,
    FillPaused = 0b00001000,
    LiqPaused = 0b00010000,
    FundingPaused = 0b00100000,
    SettlePnlPaused = 0b01000000,
    AmmImmediateFillPaused = 0b10000000,
    // Paused = 0b11111111
}

impl ExchangeStatus {
    pub fn active() -> u8 {
        BitFlags::<ExchangeStatus>::empty().bits() as u8
    }
}

/// Pause flags for internal solvency-repair flows, stored in `State::solvency_status`.
/// Kept separate from `ExchangeStatus` (which is a full u8) so repair can be gated
/// independently of user withdrawals.
#[derive(BitFlags, Clone, Copy, PartialEq, Debug, Eq)]
pub enum SolvencyStatus {
    // Active = 0b00000000
    SolvencyRepairPaused = 0b00000001,
    // Paused = 0b11111111
}

impl SolvencyStatus {
    pub fn active() -> u8 {
        BitFlags::<SolvencyStatus>::empty().bits() as u8
    }
}

impl Default for State {
    fn default() -> Self {
        State {
            cold_admin: Pubkey::default(),
            warm_admin: Pubkey::default(),
            pause_admin: Pubkey::default(),
            hot_amm_crank: Pubkey::default(),
            hot_lp_cache: Pubkey::default(),
            hot_lp_swap: Pubkey::default(),
            hot_lp_settle: Pubkey::default(),
            hot_feature_flag: Pubkey::default(),
            hot_fuel: Pubkey::default(),
            hot_user_flag: Pubkey::default(),
            hot_vault_deposit: Pubkey::default(),
            hot_mm_oracle_crank: Pubkey::default(),
            hot_amm_spread_adjust: Pubkey::default(),
            whitelist_mint: Pubkey::default(),
            discount_mint: Pubkey::default(),
            signer: Pubkey::default(),
            srm_vault: Pubkey::default(),
            protocol_fee_recipient_perp: Pubkey::default(),
            hot_fee_withdraw: Pubkey::default(),
            hot_account_extension: Pubkey::default(),
            protocol_fee_recipient_spot: Pubkey::default(),
            perp_fee_structure: FeeStructure::default(),
            spot_fee_structure: FeeStructure::default(),
            oracle_guard_rails: OracleGuardRails::default(),
            number_of_authorities: 0,
            number_of_sub_accounts: 0,
            liquidation_margin_buffer_ratio: 0,
            settlement_duration: 0,
            number_of_markets: 0,
            number_of_spot_markets: 0,
            signer_nonce: 0,
            min_perp_auction_duration: legacy_slot_duration_u8(0),
            default_market_order_time_in_force: 0,
            default_spot_auction_duration: 0,
            exchange_status: 0,
            liquidation_duration: legacy_slot_duration_u8(0),
            initial_pct_to_liquidate: 0,
            max_number_of_sub_accounts: 0,
            max_initialize_user_fee: 0,
            feature_bit_flags: 0,
            lp_pool_feature_bit_flags: 0,
            solvency_status: 0,
            promo_fee_tier: 0,
            slot_duration_ms: 0,
            pending_slot_duration_ms: 0,
            slot_duration_pad: [0; 2],
            slot_duration_effective_slot: 0,
            slot_duration_transition_slots: [0; 4],
            hot_vamm_quote_management: Pubkey::default(),
            padding: [0; 168],
        }
    }
}

impl State {
    /// Full slot clock, including every synchronized IBRL transition.
    pub fn slot_clock(&self) -> SlotClock {
        SlotClock::from_state_fields(
            self.slot_duration_transition_slots,
            self.slot_duration_ms,
            self.pending_slot_duration_ms,
            self.slot_duration_effective_slot,
        )
    }

    /// The live slot length, applying a staged switch once its effective slot has
    /// passed. Reads the current slot from the Clock sysvar so every existing
    /// caller keeps its signature; if the sysvar is unavailable (unit tests) it
    /// falls back to the pre-switch base value. The `0` (pre-upgrade / unset)
    /// sentinel resolves to the 400ms baseline.
    pub fn slot_duration(&self) -> SlotDuration {
        let now_slot = Clock::get().map(|c| c.slot).unwrap_or(0);
        self.slot_clock().slot_duration_at(now_slot)
    }

    /// The raw `slot_duration_ms` in effect at `now_slot`: the staged
    /// `pending_slot_duration_ms` once `slot_duration_effective_slot` has been
    /// reached, otherwise the current base value. `now_slot == 0` (no clock)
    /// yields the base, so pre-switch is the safe default. Shared by
    /// [`State::slot_duration`] and the native fast-path reader.
    pub fn active_slot_duration_ms(&self, now_slot: u64) -> u16 {
        self.slot_clock().slot_duration_at(now_slot).as_ms() as u16
    }

    /// Read the live slot duration from a foreign, unchecked `State` account.
    /// Thin wrapper over [`State::slot_clock_from_account_info`].
    pub fn slot_duration_from_account_info(
        account: &AccountInfo,
        now_slot: u64,
    ) -> Result<SlotDuration> {
        Ok(Self::slot_clock_from_account_info(account)?.slot_duration_at(now_slot))
    }

    /// Read the full slot clock from a foreign, unchecked `State` account, for
    /// programs (e.g. `vaults`) that hold velocity's State as a bare `AccountInfo`
    /// and cannot use `AccountLoader` — its `try_from` requires a `&'info`
    /// borrow that an `#[derive(Accounts)]` struct field cannot provide. Validates
    /// the velocity-program owner and the `State` discriminator (together unique
    /// to the singleton State), then reads the transition archive and the legacy
    /// staging fields by offset. Offsets come from `offset_of!` so they cannot
    /// drift from the layout.
    pub fn slot_clock_from_account_info(account: &AccountInfo) -> Result<SlotClock> {
        // velocity's ErrorCode (the `validate!` macro binds `ErrorCode`
        // unqualified; the anchor prelude otherwise shadows it here)
        use crate::error::ErrorCode;
        let (expected_state, _) = Pubkey::find_program_address(&[b"velocity_state"], &crate::id());
        crate::validate!(
            account.key == &expected_state,
            ErrorCode::DefaultError,
            "account is not the velocity State PDA"
        )?;
        crate::validate!(
            account.owner == &crate::id(),
            ErrorCode::DefaultError,
            "State account not owned by the velocity program"
        )?;
        let data = account.try_borrow_data()?;
        crate::validate!(
            data.starts_with(&State::DISCRIMINATOR),
            ErrorCode::DefaultError,
            "account is not a velocity State account"
        )?;
        const DISC: usize = 8;
        let base_off = DISC + std::mem::offset_of!(State, slot_duration_ms);
        let pending_off = DISC + std::mem::offset_of!(State, pending_slot_duration_ms);
        let eff_off = DISC + std::mem::offset_of!(State, slot_duration_effective_slot);
        let transitions_off = DISC + std::mem::offset_of!(State, slot_duration_transition_slots);
        // one bounds check covers every field read below
        crate::validate!(
            data.len() >= transitions_off + 32,
            ErrorCode::DefaultError,
            "velocity State account data too short"
        )?;
        let base = u16::from_le_bytes([data[base_off], data[base_off + 1]]);
        let pending = u16::from_le_bytes([data[pending_off], data[pending_off + 1]]);
        let mut eff = [0u8; 8];
        eff.copy_from_slice(&data[eff_off..eff_off + 8]);
        let effective = u64::from_le_bytes(eff);
        let mut transition_slots = [0u64; 4];
        for (i, transition_slot) in transition_slots.iter_mut().enumerate() {
            let off = transitions_off + i * 8;
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&data[off..off + 8]);
            *transition_slot = u64::from_le_bytes(bytes);
        }
        Ok(SlotClock::from_state_fields(
            transition_slots,
            base,
            pending,
            effective,
        ))
    }

    /// `min_perp_auction_duration` as a wall-clock duration (stored in legacy
    /// 400ms units).
    pub fn min_perp_auction_duration_ms(&self) -> Millis {
        legacy_slot_duration_u8_to_millis(self.min_perp_auction_duration)
    }

    /// `liquidation_duration` (the ramp to 100% liquidatable) as a wall-clock
    /// duration (stored in legacy 400ms units).
    pub fn liquidation_duration_ms(&self) -> Millis {
        legacy_slot_duration_u8_to_millis(self.liquidation_duration)
    }

    /// The time after `PerpMarket.expiry_ts` that must pass before an expired market may move its
    /// pools to the revenue pool and delist.
    ///
    /// The window has two jobs. It lets every expired position settle. It also gives a
    /// revenue-share beneficiary time to create the payout account that `settle_revenue_share`
    /// needs, because `forfeit_revenue_share_order` writes off a row with no such account once the
    /// window ends. A `settlement_duration` of 1 shortens the window for tests.
    pub fn escrow_period_before_transfer(&self) -> VelocityResult<i64> {
        if self.settlement_duration > 1 {
            // At least TWENTY_FOUR_HOUR, so that an operator can examine the settlement.
            TWENTY_FOUR_HOUR
                .safe_add(self.settlement_duration.cast()?)?
                .safe_sub(1)
        } else {
            self.settlement_duration.cast::<i64>()
        }
    }

    pub fn get_exchange_status(&self) -> VelocityResult<BitFlags<ExchangeStatus>> {
        BitFlags::<ExchangeStatus>::from_bits(usize::from(self.exchange_status)).safe_unwrap()
    }

    pub fn amm_immediate_fill_paused(&self) -> VelocityResult<bool> {
        Ok(self
            .get_exchange_status()?
            .contains(ExchangeStatus::AmmImmediateFillPaused))
    }

    pub fn amm_paused(&self) -> VelocityResult<bool> {
        Ok(self
            .get_exchange_status()?
            .contains(ExchangeStatus::AmmPaused))
    }

    pub fn funding_paused(&self) -> VelocityResult<bool> {
        Ok(self
            .get_exchange_status()?
            .contains(ExchangeStatus::FundingPaused))
    }

    pub fn withdraw_paused(&self) -> VelocityResult<bool> {
        Ok(self
            .get_exchange_status()?
            .contains(ExchangeStatus::WithdrawPaused))
    }

    pub fn deposit_paused(&self) -> VelocityResult<bool> {
        Ok(self
            .get_exchange_status()?
            .contains(ExchangeStatus::DepositPaused))
    }

    pub fn get_solvency_status(&self) -> VelocityResult<BitFlags<SolvencyStatus>> {
        BitFlags::<SolvencyStatus>::from_bits(usize::from(self.solvency_status)).safe_unwrap()
    }

    pub fn solvency_repair_paused(&self) -> VelocityResult<bool> {
        Ok(self
            .get_solvency_status()?
            .contains(SolvencyStatus::SolvencyRepairPaused))
    }

    pub fn max_number_of_sub_accounts(&self) -> u64 {
        if self.max_number_of_sub_accounts <= 5 {
            return self.max_number_of_sub_accounts as u64;
        }

        (self.max_number_of_sub_accounts as u64).saturating_mul(100)
    }

    pub fn get_init_user_fee(&self) -> VelocityResult<u64> {
        let max_init_fee: u64 = (self.max_initialize_user_fee as u64) * LAMPORTS_PER_SOL_U64 / 100;

        let target_utilization: u64 = 8 * PERCENTAGE_PRECISION_U64 / 10;

        let account_space_utilization: u64 = self
            .number_of_sub_accounts
            .safe_mul(PERCENTAGE_PRECISION_U64)?
            .safe_div(self.max_number_of_sub_accounts().max(1))?;

        let init_fee: u64 = if account_space_utilization > target_utilization {
            max_init_fee
                .safe_mul(account_space_utilization.safe_sub(target_utilization)?)?
                .safe_div(PERCENTAGE_PRECISION_U64.safe_sub(target_utilization)?)?
        } else {
            0
        };

        Ok(init_fee)
    }

    pub fn use_median_trigger_price(&self) -> bool {
        (self.feature_bit_flags & (FeatureBitFlags::MedianTriggerPrice as u8)) > 0
    }

    pub fn builder_codes_enabled(&self) -> bool {
        (self.feature_bit_flags & (FeatureBitFlags::BuilderCodes as u8)) > 0
    }

    pub fn vamm_maker_rebate_enabled(&self) -> bool {
        (self.feature_bit_flags & (FeatureBitFlags::VammMakerRebate as u8)) > 0
    }

    pub fn allow_settle_lp_pool(&self) -> bool {
        (self.lp_pool_feature_bit_flags & (LpPoolFeatureBitFlags::SettleLpPool as u8)) > 0
    }

    pub fn allow_swap_lp_pool(&self) -> bool {
        (self.lp_pool_feature_bit_flags & (LpPoolFeatureBitFlags::SwapLpPool as u8)) > 0
    }

    pub fn allow_mint_redeem_lp_pool(&self) -> bool {
        (self.lp_pool_feature_bit_flags & (LpPoolFeatureBitFlags::MintRedeemLpPool as u8)) > 0
    }

    /// Pubkey assigned to a given hot role. `Pubkey::default()` if unassigned.
    pub fn hot_key(&self, role: HotRole) -> Pubkey {
        match role {
            HotRole::AmmCrank => self.hot_amm_crank,
            HotRole::LpCache => self.hot_lp_cache,
            HotRole::LpSwap => self.hot_lp_swap,
            HotRole::LpSettle => self.hot_lp_settle,
            HotRole::FeatureFlag => self.hot_feature_flag,
            HotRole::Fuel => self.hot_fuel,
            HotRole::UserFlag => self.hot_user_flag,
            HotRole::VaultDeposit => self.hot_vault_deposit,
            HotRole::MmOracleCrank => self.hot_mm_oracle_crank,
            HotRole::AmmSpreadAdjust => self.hot_amm_spread_adjust,
            HotRole::FeeWithdraw => self.hot_fee_withdraw,
            HotRole::AccountExtension => self.hot_account_extension,
            HotRole::VammQuoteManagement => self.hot_vamm_quote_management,
        }
    }

    pub fn set_hot_key(&mut self, role: HotRole, key: Pubkey) {
        match role {
            HotRole::AmmCrank => self.hot_amm_crank = key,
            HotRole::LpCache => self.hot_lp_cache = key,
            HotRole::LpSwap => self.hot_lp_swap = key,
            HotRole::LpSettle => self.hot_lp_settle = key,
            HotRole::FeatureFlag => self.hot_feature_flag = key,
            HotRole::Fuel => self.hot_fuel = key,
            HotRole::UserFlag => self.hot_user_flag = key,
            HotRole::VaultDeposit => self.hot_vault_deposit = key,
            HotRole::MmOracleCrank => self.hot_mm_oracle_crank = key,
            HotRole::AmmSpreadAdjust => self.hot_amm_spread_adjust = key,
            HotRole::FeeWithdraw => self.hot_fee_withdraw = key,
            HotRole::AccountExtension => self.hot_account_extension = key,
            HotRole::VammQuoteManagement => self.hot_vamm_quote_management = key,
        }
    }

    /// True if `signer` is the root (cold) admin. Mirrors the
    /// `cold_admin == admin.key()` constraint used by the cold-only Accounts
    /// structs (`ColdAdminUpdateState`, `UpdateWarmAdmin`, `UpdatePauseAdmin`),
    /// so a cold signer is recognised even when `warm_admin` is set — including
    /// the post-`handle_initialize` state where `warm_admin == cold_admin`.
    pub fn is_cold(&self, signer: &Pubkey) -> bool {
        self.cold_admin == *signer && self.cold_admin != Pubkey::default()
    }

    pub fn is_warm(&self, signer: &Pubkey) -> bool {
        self.is_cold(signer) || (self.warm_admin != Pubkey::default() && self.warm_admin == *signer)
    }

    /// True if `signer` can flip pause flags: cold, warm, or the dedicated
    /// fast-path pause admin. The pause path intentionally bypasses any warm
    /// timelock so a compromised market can be halted without delay.
    pub fn is_pause(&self, signer: &Pubkey) -> bool {
        self.is_warm(signer)
            || (self.pause_admin != Pubkey::default() && self.pause_admin == *signer)
    }

    pub fn is_hot(&self, signer: &Pubkey, role: HotRole) -> bool {
        if self.is_warm(signer) {
            return true;
        }
        let role_key = self.hot_key(role);
        role_key != Pubkey::default() && role_key == *signer
    }

    pub fn require_cold(&self, signer: &Pubkey) -> VelocityResult<()> {
        if !self.is_cold(signer) {
            msg!("signer {} is not cold admin", signer);
            return Err(crate::error::ErrorCode::Unauthorized);
        }
        Ok(())
    }

    pub fn require_warm(&self, signer: &Pubkey) -> VelocityResult<()> {
        if !self.is_warm(signer) {
            msg!("signer {} is neither cold nor warm admin", signer);
            return Err(crate::error::ErrorCode::Unauthorized);
        }
        Ok(())
    }

    pub fn require_pause(&self, signer: &Pubkey) -> VelocityResult<()> {
        if !self.is_pause(signer) {
            msg!("signer {} is not authorized to flip pause flags", signer);
            return Err(crate::error::ErrorCode::Unauthorized);
        }
        Ok(())
    }

    pub fn require_hot(&self, signer: &Pubkey, role: HotRole) -> VelocityResult<()> {
        if !self.is_hot(signer, role) {
            msg!("signer {} is not authorized for role {:?}", signer, role);
            return Err(crate::error::ErrorCode::Unauthorized);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Eq)]
pub enum FeatureBitFlags {
    MmOracleUpdate = 0b00000001,
    MedianTriggerPrice = 0b00000010,
    BuilderCodes = 0b00000100,
    VammMakerRebate = 0b00001000,
}

#[derive(Clone, Copy, PartialEq, Debug, Eq)]
pub enum LpPoolFeatureBitFlags {
    SettleLpPool = 0b00000001,
    SwapLpPool = 0b00000010,
    MintRedeemLpPool = 0b00000100,
}

impl Size for State {
    // 8 (disc) + 13 Pubkey (cold + warm + pause + 10 hot, 416 B) + 8 Pubkey (mint/signer/srm
    // + protocol_fee_recipient_perp/_spot + hot_fee_withdraw + hot_account_extension, 256 B)
    // + 2*FeeStructure + OracleGuardRails + scalars + solvency_status[1] + promo_fee_tier[1]
    // + slot_duration_ms[2] + pending_slot_duration_ms[2] + slot_duration_pad[2]
    // + slot_duration_effective_slot[8] + transition slots[32]
    // + hot_vamm_quote_management[32] + padding[168] = 1752 B.
    // The quote management key starts at struct offset 1544, where padding began.
    // hot_if_rebalance was removed with the if-rebalance machinery (its 32 B went into
    // the padding); protocol_fee_recipient_spot later took 32 B back out; solvency_status
    // took 1 B out of the padding; hot_account_extension took another 32 B out;
    // slot_duration_ms took 2 B out (promo_fee_tier ends at an odd offset, so the u16
    // starts at the even byte right after it — no implicit padding, pinned below); the
    // staging fields (pending_slot_duration_ms[2] + slot_duration_pad[2] +
    // slot_duration_effective_slot[8]) took another 12 B out; quote management took
    // another 32 B; and the padding absorbed the 8 formerly-implicit trailing bytes
    // (see the field doc) so sizeof is target-independent.
    // SIZE stays constant and (SIZE - 8) % 16 == 0 holds (1744).
    const SIZE: usize = 1752;
}

// `slot_duration_ms` must start exactly where the old padding began (byte 1498
// of the struct, an even offset), so pre-upgrade accounts read `0` (= 400ms
// baseline) out of former padding. The staging fields follow it with explicit
// padding so `slot_duration_effective_slot` (u64) lands 8-aligned at 1504 with no
// implicit alignment padding. The size assert holds on both x86_64 (u128 align
// 16) and SBF (u128 align 8) because all padding is explicit.
static_assertions::const_assert_eq!(std::mem::offset_of!(State, slot_duration_ms), 1498);
static_assertions::const_assert_eq!(std::mem::offset_of!(State, pending_slot_duration_ms), 1500);
static_assertions::const_assert_eq!(
    std::mem::offset_of!(State, slot_duration_effective_slot),
    1504
);
static_assertions::const_assert_eq!(
    std::mem::offset_of!(State, slot_duration_transition_slots),
    1512
);
static_assertions::const_assert_eq!(std::mem::size_of::<State>(), 1744);

#[derive(Copy, AnchorSerialize, AnchorDeserialize, Clone, Debug)]
#[repr(C)]
pub struct OracleGuardRails {
    pub price_divergence: PriceDivergenceGuardRails,
    pub validity: ValidityGuardRails,
}

impl Default for OracleGuardRails {
    fn default() -> Self {
        OracleGuardRails {
            price_divergence: PriceDivergenceGuardRails::default(),
            validity: ValidityGuardRails {
                slots_before_stale_for_amm: legacy_slot_duration_i64(10), // 4s
                slots_before_stale_for_margin: legacy_slot_duration_i64(120), // 48s
                confidence_interval_max_size: 20_000,                     // 2% of price
                too_volatile_ratio: 5,                                    // 5x or 80% down
            },
        }
    }
}

impl OracleGuardRails {
    pub fn max_oracle_twap_5min_percent_divergence(&self) -> u64 {
        self.price_divergence
            .oracle_twap_5min_percent_divergence
            .max(PERCENTAGE_PRECISION_U64 / 2)
    }
}

#[derive(Copy, AnchorSerialize, AnchorDeserialize, Clone, Debug)]
#[repr(C)]
pub struct PriceDivergenceGuardRails {
    pub mark_oracle_percent_divergence: u64,
    pub oracle_twap_5min_percent_divergence: u64,
}

impl Default for PriceDivergenceGuardRails {
    fn default() -> Self {
        PriceDivergenceGuardRails {
            mark_oracle_percent_divergence: PERCENTAGE_PRECISION_U64 / 10,
            oracle_twap_5min_percent_divergence: PERCENTAGE_PRECISION_U64 / 2,
        }
    }
}

#[derive(Copy, AnchorSerialize, AnchorDeserialize, Clone, Default, Debug)]
#[repr(C)]
pub struct ValidityGuardRails {
    /// Compact wall-clock duration encoded in historical 400ms slot quanta.
    pub slots_before_stale_for_amm: LegacySlotDurationI64,
    /// Compact wall-clock duration encoded in historical 400ms slot quanta.
    pub slots_before_stale_for_margin: LegacySlotDurationI64,
    pub confidence_interval_max_size: u64,
    pub too_volatile_ratio: i64,
}

impl ValidityGuardRails {
    /// AMM staleness window as a wall-clock duration.
    pub fn stale_for_amm_ms(&self) -> Millis {
        legacy_slot_duration_i64_to_millis(self.slots_before_stale_for_amm)
    }

    /// Margin staleness window as a wall-clock duration.
    pub fn stale_for_margin_ms(&self) -> Millis {
        legacy_slot_duration_i64_to_millis(self.slots_before_stale_for_margin)
    }
}

#[derive(Copy, AnchorSerialize, AnchorDeserialize, Clone, Debug)]
#[repr(C)]
pub struct FeeStructure {
    pub fee_tiers: [FeeTier; 10],
    pub filler_reward_structure: OrderFillerRewardStructure,
    pub flat_filler_fee: u64,
    /// Share of the trade-fee *remainder* (taker fee after maker rebate, referral,
    /// referee discount, and filler reward are taken off the top) provisioned to
    /// the AMM as liquidity (its backstop-of-last-resort tranche, tracked in
    /// `PerpMarket.fee_ledger.amm_protocol_fees_received` alongside the vAMM
    /// maker rebate when that feature is enabled). precision:
    /// FEE_PERCENTAGE_DENOMINATOR. `amm_fee_numerator + if_fee_numerator` must
    /// be <= FEE_PERCENTAGE_DENOMINATOR; the protocol receives the residual
    /// (`remainder − amm − if`) into its withdrawable `protocol_fee_pool`.
    /// (Was the reserved `padding: u64`, repartitioned into two u32s —
    /// size/alignment unchanged.)
    pub amm_fee_numerator: u32,
    /// Share of the trade-fee remainder routed to the insurance fund (`revenue_pool`).
    pub if_fee_numerator: u32,
}

impl Default for FeeStructure {
    fn default() -> Self {
        FeeStructure::perps_default()
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Copy, Clone, Debug)]
#[repr(C)]
pub struct FeeTier {
    pub fee_numerator: u32,
    pub fee_denominator: u32,
    pub maker_rebate_numerator: u32,
    pub maker_rebate_denominator: u32,
    pub referrer_reward_numerator: u32,
    pub referrer_reward_denominator: u32,
    pub referee_fee_numerator: u32,
    pub referee_fee_denominator: u32,
}

impl Default for FeeTier {
    fn default() -> Self {
        FeeTier {
            fee_numerator: 0,
            fee_denominator: FEE_DENOMINATOR,
            maker_rebate_numerator: 0,
            maker_rebate_denominator: FEE_DENOMINATOR,
            referrer_reward_numerator: 0,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR,
            referee_fee_numerator: 0,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR,
        }
    }
}

/// `u128` is placed first so `#[repr(C)]` layout matches between host (x86_64,
/// align 16 in Rust ≥ 1.77) and the SBF VM (align 8). Trailing `_padding`
/// rounds the struct to a host-portable 32 bytes. See
/// `docs/alignment-and-native-offsets.md`.
#[derive(AnchorSerialize, AnchorDeserialize, Copy, Default, Clone, Debug)]
#[repr(C)]
pub struct OrderFillerRewardStructure {
    pub time_based_reward_lower_bound: u128,
    pub reward_numerator: u32,
    pub reward_denominator: u32,
    pub _padding: [u8; 8],
}

impl FeeStructure {
    /// Three volume tiers (see `determine_perp_fee_tier` for the 30d-volume
    /// thresholds): 4bps / 3bps / 2bps taker, flat -0.25bp maker rebate.
    /// Per-market absolute surcharges/discounts (e.g. volatile-alt add-ons)
    /// live on `PerpMarket.taker_fee_addon_tenth_bps`, not in the tiers.
    pub fn perps_default() -> Self {
        let mut fee_tiers = [FeeTier::default(); 10];
        fee_tiers[0] = FeeTier {
            fee_numerator: 40,
            fee_denominator: FEE_DENOMINATOR, // 4 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: 10 * FEE_DENOMINATOR, // 0.25bp
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        fee_tiers[1] = FeeTier {
            fee_numerator: 30,
            fee_denominator: FEE_DENOMINATOR, // 3 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: 10 * FEE_DENOMINATOR, // 0.25bp
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        fee_tiers[2] = FeeTier {
            fee_numerator: 20,
            fee_denominator: FEE_DENOMINATOR, // 2 bps
            maker_rebate_numerator: 25,
            maker_rebate_denominator: 10 * FEE_DENOMINATOR, // 0.25bp
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 10% of taker fee
            referee_fee_numerator: 5,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 5%
        };
        FeeStructure {
            fee_tiers,
            filler_reward_structure: OrderFillerRewardStructure {
                time_based_reward_lower_bound: 10_000, // 1 cent
                reward_numerator: 10,
                reward_denominator: FEE_PERCENTAGE_DENOMINATOR,
                _padding: [0; 8],
            },
            flat_filler_fee: 10_000,
            // default: 0% to the AMM and 0% to the IF — the protocol (the
            // residual claimant) receives 100% of the net trade-fee remainder.
            // Admin sets the AMM/IF shares explicitly.
            amm_fee_numerator: 0,
            if_fee_numerator: 0,
        }
    }

    pub fn spot_default() -> Self {
        let mut fee_tiers = [FeeTier::default(); 10];
        fee_tiers[0] = FeeTier {
            fee_numerator: 100,
            fee_denominator: FEE_DENOMINATOR, // 10 bps
            maker_rebate_numerator: 20,
            maker_rebate_denominator: FEE_DENOMINATOR, // 2bps
            referrer_reward_numerator: 0,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR, // 0% of taker fee
            referee_fee_numerator: 0,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR, // 0%
        };
        FeeStructure {
            fee_tiers,
            filler_reward_structure: OrderFillerRewardStructure {
                time_based_reward_lower_bound: 10_000, // 1 cent
                reward_numerator: 10,
                reward_denominator: FEE_PERCENTAGE_DENOMINATOR,
                _padding: [0; 8],
            },
            flat_filler_fee: 10_000,
            amm_fee_numerator: 0,
            if_fee_numerator: 0,
        }
    }
}

#[cfg(test)]
impl FeeStructure {
    pub fn test_default() -> Self {
        let mut fee_tiers = [FeeTier::default(); 10];
        fee_tiers[0] = FeeTier {
            fee_numerator: 100,
            fee_denominator: FEE_DENOMINATOR,
            maker_rebate_numerator: 60,
            maker_rebate_denominator: FEE_DENOMINATOR,
            referrer_reward_numerator: 10,
            referrer_reward_denominator: FEE_PERCENTAGE_DENOMINATOR,
            referee_fee_numerator: 10,
            referee_fee_denominator: FEE_PERCENTAGE_DENOMINATOR,
        };
        FeeStructure {
            fee_tiers,
            filler_reward_structure: OrderFillerRewardStructure {
                time_based_reward_lower_bound: 10_000, // 1 cent
                reward_numerator: 10,
                reward_denominator: FEE_PERCENTAGE_DENOMINATOR,
                _padding: [0; 8],
            },
            ..FeeStructure::perps_default()
        }
    }
}
