use {
    super::oracle_map::OracleIdentifier,
    crate::{
        error::{ErrorCode, VelocityResult},
        math::{
            casting::Cast,
            constants::{
                AMM_TO_QUOTE_PRECISION_RATIO, BANKRUPTCY_IF_FLOOR_DISABLED, BASE_PRECISION,
                DEFAULT_BANKRUPTCY_IF_FLOOR_PCT, DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT,
                FUNDING_RATE_BUFFER_I128, FUNDING_RATE_OFFSET_PERCENTAGE,
                LIQUIDATION_FEE_PRECISION, MARGIN_PRECISION, MARGIN_PRECISION_U128,
                MAX_LIQUIDATION_MULTIPLIER, ONE_MINUTE, PERCENTAGE_PRECISION,
                PERCENTAGE_PRECISION_I128, PERCENTAGE_PRECISION_I64, PERCENTAGE_PRECISION_U32,
                PERCENTAGE_PRECISION_U64, PRICE_PRECISION_I128, SPOT_WEIGHT_PRECISION,
                TRIGGER_PRICE_LAST_FILL_MAX_AGE,
            },
            margin::{
                calculate_size_discount_asset_weight, calculate_size_premium_liability_weight,
                MarginRequirementType,
            },
            oracle::{
                is_oracle_valid_for_action, oracle_validity, LogMode, OracleValidity,
                VelocityAction,
            },
            safe_math::SafeMath,
            time::SlotDuration,
        },
        msg,
        state::{
            fill_mode::FillMode,
            market_status::MarketStatus,
            oracle::{HistoricalOracleData, MMOraclePriceData, OraclePriceData, OracleSource},
            paused_operations::PerpOperation,
            spot_market::{AssetTier, SpotBalance, SpotBalanceType},
            state::{State, ValidityGuardRails},
            traits::{MarketIndexOffset, Size},
            user::{MarketType, Order},
        },
        validate,
        vlp::amm::math::amm::{
            self, calculate_new_oracle_price_twap, sanitize_new_price, TwapPeriod,
        },
    },
    anchor_lang::prelude::{
        borsh::{BorshDeserialize, BorshSerialize},
        *,
    },
    std::cmp::{max, min},
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq, Debug, Eq, Default)]
pub enum LpStatus {
    /// Not considered
    #[default]
    Uncollateralized,
    /// all operations allowed
    Active,
    /// Decommissioning
    Decommissioning,
}

impl LpStatus {
    pub fn is_collateralized(&self) -> bool {
        !matches!(self, LpStatus::Uncollateralized)
    }
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq, Default)]
pub enum ContractType {
    #[default]
    Perpetual,
    DeprecatedFuture,
    DeprecatedPrediction,
}

#[derive(
    Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq, PartialOrd, Ord, Default,
)]
pub enum ContractTier {
    /// max insurance capped at A level
    A,
    /// max insurance capped at B level
    B,
    /// max insurance capped at C level
    C,
    /// no insurance
    Speculative,
    /// no insurance, another tranches below
    #[default]
    HighlySpeculative,
    /// no insurance, only single position allowed
    Isolated,
}

impl ContractTier {
    pub fn is_as_safe_as(&self, best_contract: &ContractTier, best_asset: &AssetTier) -> bool {
        self.is_as_safe_as_contract(best_contract) && self.is_as_safe_as_asset(best_asset)
    }

    pub fn is_as_safe_as_contract(&self, other: &ContractTier) -> bool {
        // Contract Tier A safest
        self <= other
    }
    pub fn is_as_safe_as_asset(&self, other: &AssetTier) -> bool {
        // allow Contract Tier A,B,C to rank above Assets below Collateral status
        if other == &AssetTier::Unlisted {
            true
        } else {
            other >= &AssetTier::Cross && self <= &ContractTier::C
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Eq)]
pub enum MarketConfigFlag {
    DisableFormulaicKUpdate = 0b00000001,
}

/// All of a perp market's fee-split accounting in one ledger.
/// Pure counters — token claims live in the pools
/// (`protocol_fee_pool`, the quote `revenue_pool`, `AMM.fee_pool`).
/// Convention: gross-fee counters record what the taker actually paid
/// (post referee discount, pre carve-outs) on BOTH the AMM and DLOB-match
/// paths.
#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct FeeLedger {
    /// lifetime gross taker fees collected (analytics; not a routing driver)
    /// precision: QUOTE_PRECISION
    pub total_exchange_fee: u128,
    /// lifetime liquidation fees charged to liquidatees (IF + protocol cuts;
    /// pure analytics — routing happens via the pending counters).
    /// precision: QUOTE_PRECISION
    pub total_liquidation_fee: u128,
    /// protocol (residual) carveouts accrued but not yet materialized into
    /// `protocol_fee_pool`. precision: QUOTE_PRECISION
    pub pending_protocol_fee: u128,
    /// insurance-fund carveouts accrued but not yet materialized into the
    /// quote `revenue_pool`; also the first bankruptcy tranche.
    /// precision: QUOTE_PRECISION
    pub pending_if_fee: u128,
    /// cumulative fee provision granted to the AMM via `amm_fee_numerator`,
    /// plus the vAMM maker rebate when `FeatureBitFlags::VammMakerRebate` is
    /// enabled — its backstop-of-last-resort tranche, drawable (and
    /// decremented) only in bankruptcy. Enabling the rebate bit therefore
    /// grows the bankruptcy clawback cap by the rebates earned. The AMM's own
    /// spread/trading capital beyond this provision is never tapped.
    /// precision: QUOTE_PRECISION
    pub amm_protocol_fees_received: u128,
    /// AMM fee provision (including the vAMM maker rebate when enabled)
    /// accrued at fill (already booked into the AMM's
    /// `total_fee_minus_distributions`) but not yet tokenized into
    /// `amm.fee_pool` by the sweep. Invariant: `<= amm_protocol_fees_received`.
    /// precision: QUOTE_PRECISION
    pub pending_amm_provision: u128,
}

impl FeeLedger {
    /// Record a fill: gross taker fee for analytics plus the three-way split
    /// carveouts (`protocol` pending, `if` pending, `amm` provision — the
    /// last both grows the lifetime clawback cap and queues for tokenization).
    pub fn accrue_fill_fees(
        &mut self,
        gross_taker_fee: u64,
        protocol_fee: u64,
        if_fee: u64,
        amm_fee: u64,
    ) -> VelocityResult {
        self.total_exchange_fee = self.total_exchange_fee.safe_add(gross_taker_fee.cast()?)?;
        self.pending_protocol_fee = self.pending_protocol_fee.safe_add(protocol_fee.cast()?)?;
        self.pending_if_fee = self.pending_if_fee.safe_add(if_fee.cast()?)?;
        self.amm_protocol_fees_received =
            self.amm_protocol_fees_received.safe_add(amm_fee.cast()?)?;
        self.pending_amm_provision = self.pending_amm_provision.safe_add(amm_fee.cast()?)?;
        Ok(())
    }

    /// Record a liquidation's IF and protocol cuts. Both debit the liquidatee
    /// without crediting the AMM's books, so both accumulate into
    /// `total_liquidation_fee` (see its field doc).
    pub fn accrue_liquidation_fees(&mut self, if_fee: u64, protocol_fee: u64) -> VelocityResult {
        self.total_liquidation_fee = self
            .total_liquidation_fee
            .safe_add(if_fee.cast()?)?
            .safe_add(protocol_fee.cast()?)?;
        self.pending_if_fee = self.pending_if_fee.safe_add(if_fee.cast()?)?;
        self.pending_protocol_fee = self.pending_protocol_fee.safe_add(protocol_fee.cast()?)?;
        Ok(())
    }

    /// Move a bankrupt estate's forfeited perp claim to this pool's insurance tranche (OtterSec #145).
    ///
    /// This is not `accrue_liquidation_fees`, which also raises `total_liquidation_fee`. Nobody
    /// charged a fee here. Only the creditor changed, from a bankrupt user to the insurance fund.
    ///
    /// `pending_if_fee` is already a claim on future PnL-pool inflows, which is what the user's
    /// unfundable claim was. The swap is therefore equity-neutral: zeroing the user's
    /// `quote_asset_amount` lowers `market.quote_asset_amount`, and so `net_user_pnl`, which raises the
    /// market's excess by the amount this subtracts.
    pub fn accrue_forfeited_claim_to_if(&mut self, amount: u128) -> VelocityResult {
        self.pending_if_fee = self.pending_if_fee.safe_add(amount)?;
        Ok(())
    }

    /// Fees accrued but not yet materialized — the floor funding/spending may
    /// not eat into.
    pub fn pending_fee_obligations(&self) -> VelocityResult<u128> {
        self.pending_protocol_fee.safe_add(self.pending_if_fee)
    }

    pub fn consume_pending_if(&mut self, amount: u128) -> VelocityResult {
        self.pending_if_fee = self.pending_if_fee.safe_sub(amount)?;
        Ok(())
    }

    pub fn consume_pending_protocol(&mut self, amount: u128) -> VelocityResult {
        self.pending_protocol_fee = self.pending_protocol_fee.safe_sub(amount)?;
        Ok(())
    }

    /// Draw down the AMM's provisioned tranche (bankruptcy backstop of last
    /// resort).
    pub fn consume_amm_backstop(&mut self, amount: u128) -> VelocityResult {
        self.amm_protocol_fees_received = self.amm_protocol_fees_received.safe_sub(amount)?;
        Ok(())
    }

    /// Mark accrued AMM provision as tokenized into `amm.fee_pool` (sweep) or
    /// consumed by a bankruptcy clawback before tokenization.
    pub fn consume_pending_amm_provision(&mut self, amount: u128) -> VelocityResult {
        self.pending_amm_provision = self.pending_amm_provision.safe_sub(amount)?;
        Ok(())
    }
}

#[account(zero_copy(unsafe))]
#[derive(Eq, PartialEq, Debug)]
#[repr(C)]
pub struct PerpMarket {
    /// The perp market's address. It is a pda of the market index
    pub pubkey: Pubkey,
    // u128/i128 fields placed first so the zero-copy struct's u128 fields hit
    // 16-byte alignment. Group: protocol-wide position counters / open interest.
    /// always non-negative. tracks number of total longs in market (regardless of counterparty)
    /// precision: BASE_PRECISION
    pub base_asset_amount_long: i128,
    /// always non-positive. tracks number of total shorts in market (regardless of counterparty)
    /// precision: BASE_PRECISION
    pub base_asset_amount_short: i128,
    /// sum of all user's perp quote_asset_amount in market
    /// precision: QUOTE_PRECISION
    pub quote_asset_amount: i128,
    /// sum of all long user's quote_entry_amount in market
    /// precision: QUOTE_PRECISION
    pub quote_entry_amount_long: i128,
    /// sum of all short user's quote_entry_amount in market
    /// precision: QUOTE_PRECISION
    pub quote_entry_amount_short: i128,
    /// sum of all long user's quote_break_even_amount in market
    /// precision: QUOTE_PRECISION
    pub quote_break_even_amount_long: i128,
    /// sum of all short user's quote_break_even_amount in market
    /// precision: QUOTE_PRECISION
    pub quote_break_even_amount_short: i128,
    /// max allowed open interest, blocks trades that breach this value
    /// precision: BASE_PRECISION
    pub max_open_interest: u128,
    /// accumulated social loss paid by users since inception in market
    /// precision: QUOTE_PRECISION
    pub total_social_loss: u128,
    /// accumulated funding rate for longs since inception in market
    pub cumulative_funding_rate_long: i128,
    /// accumulated funding rate for shorts since inception in market
    pub cumulative_funding_rate_short: i128,
    /// The market's fee ledger: every fee-split counter in one place (gross
    /// analytics, pending protocol/IF carveouts, and the AMM's backstop
    /// tranche). Mutate through its accessor methods, not raw field writes.
    pub fee_ledger: FeeLedger,
    /// oracle price data public key
    pub oracle: Pubkey,
    /// The market's pnl pool. When users settle negative pnl, the balance increases.
    /// When users settle positive pnl, the balance decreases. Can not go negative.
    pub pnl_pool: PoolBalance,
    /// Protocol fees collected on this perp market, quote/USDC-denominated — a
    /// protocol-owned Deposit-type claim against the quote spot market vault
    /// (like `pnl_pool`; counted in the quote market's `deposit_balance`).
    /// Owned by the protocol, not users, and never part of the insurance
    /// backstop. `market_index` is set to `quote_spot_market_index`. Withdrawn
    /// directly to `State.protocol_fee_recipient_perp`.
    pub protocol_fee_pool: PoolBalance,
    /// Protocol's cut of a perp liquidation, taken from the liquidatee.
    /// precision: LIQUIDATOR_FEE_PRECISION
    pub protocol_liquidation_fee: u32,
    /// Additive per-market taker-fee surcharge in tenth-bps (10 = 1bp),
    /// unsigned: surcharge only (e.g. toxic-flow markets), never a discount.
    /// A discount could push the taker fee below the maker rebate it must
    /// fund and revert every match fill; promo discounts go through
    /// `State.promo_fee_tier` instead. Applied on top of the tier fee before
    /// `fee_adjustment` scales the sum:
    /// `taker_fee = (tier_fee + add-on) * (1 +/- fee_adjustment%)`.
    /// Taker fee only; the maker rebate and the post-only path see
    /// `fee_adjustment` alone. Occupies 2 bytes of the former 4-byte
    /// `_padding_buffer` (same offset/alignment on all targets), so existing
    /// accounts read 0 = no add-on until the admin sets it.
    pub taker_fee_addon_tenth_bps: u16,
    pub _padding_buffer: [u8; 2],
    /// The pnl-pool retention buffer the streaming sweep's IF and
    /// AMM-provision drains leave untouched: `sweep_market_fees` drains
    /// those pendings only from what the pnl pool holds above
    /// `max(net_user_pnl, 0) + fee_pool_buffer_target`. The protocol drain
    /// is EXEMPT — it reserves only `max(net_user_pnl, 0)` and runs first;
    /// it sweeps every settle, so each drain stays small, and its pending is
    /// no bankruptcy tranche so retaining it buys nothing.
    ///
    /// Why a buffer on top of the user-claims reservation: `net_user_pnl`
    /// is a mark-to-market snapshot, so a pool swept to the exact mark is
    /// short on the next adverse oracle tick — and the sweep is a one-way
    /// valve, so the slack can't be cheaply recalled (IF value returns only
    /// through capped gated paths, the AMM provision only via bankruptcy
    /// clawback). The buffer throttles those outflows per sweep; pool tokens
    /// are fungible (pendings are counters, not segregated tokens), so
    /// whichever cut lingers keeps settling winners in the meantime. This
    /// delays materialization, it does not divert anyone's cut. Side
    /// benefits: an unswept IF cut gives THIS market uncapped market-local
    /// bankruptcy coverage (tranche 1) instead of capped shared-vault
    /// coverage, and the buffer damps the IF settle ratchet (value settled
    /// into the IF accrues to stakers permanently).
    /// precision: QUOTE_PRECISION
    pub fee_pool_buffer_target: u64,
    /// Encoded display name for the perp market e.g. SOL-PERP
    pub name: [u8; 32],
    /// The perp market's claim on the insurance fund
    pub insurance_claim: InsuranceClaim,
    /// last funding rate in this perp market (unit is quote per base)
    /// precision: FUNDING_RATE_PRECISION
    pub last_funding_rate: i64,
    /// last funding rate for longs in this perp market (unit is quote per base)
    /// precision: FUNDING_RATE_PRECISION
    pub last_funding_rate_long: i64,
    /// last funding rate for shorts in this perp market (unit is quote per base)
    /// precision: QUOTE_PRECISION
    pub last_funding_rate_short: i64,
    /// the last funding rate update unix_timestamp
    pub last_funding_rate_ts: i64,
    /// unsettled funding pnl across the market (protocol-wide)
    pub net_unsettled_funding_pnl: i64,
    /// dead-zone threshold for the funding premium. mark/oracle twap spreads
    /// within +/- this band are treated as noise and add no premium; spreads
    /// past it are shrunk toward zero by this amount so funding stays continuous
    /// across the boundary. fit per market post-launch
    /// precision: BPS_PRECISION
    pub funding_clamp_threshold: u32,
    /// slope of the funding premium ramp above the dead zone. 1.0x passes the
    /// shrunk spread through unchanged; higher leans into the premium harder
    /// fit per market post-launch
    /// precision: PERCENTAGE_PRECISION
    pub funding_ramp_slope: u32,
    /// the base step size (increment) of orders
    /// precision: BASE_PRECISION
    pub order_step_size: u64,
    /// the price tick size of orders
    /// precision: PRICE_PRECISION
    pub order_tick_size: u64,
    /// The max pnl imbalance before positive pnl asset weight is discounted
    /// pnl imbalance is the difference between long and short pnl. When it's greater than 0,
    /// the amm has negative pnl and the initial asset weight for positive pnl is discounted
    /// precision = QUOTE_PRECISION
    pub unrealized_pnl_max_imbalance: u64,
    /// The ts when the market will be expired. Only set if market is in reduce only mode
    pub expiry_ts: i64,
    /// The price at which positions will be settled. Only set if market is expired
    /// precision = PRICE_PRECISION
    pub expiry_price: i64,
    /// Every trade has a fill record id. This is the next id to be used
    pub next_fill_record_id: u64,
    /// Every funding rate update has a record id. This is the next id to be used
    pub next_funding_rate_record_id: u64,
    /// The initial margin fraction factor. Used to increase margin ratio for large positions
    /// precision: MARGIN_PRECISION
    pub imf_factor: u32,
    /// The imf factor for unrealized pnl. Used to discount asset weight for large positive pnl
    /// precision: MARGIN_PRECISION
    pub unrealized_pnl_imf_factor: u32,
    /// The fee the liquidator is paid for taking over perp position
    /// precision: LIQUIDATOR_FEE_PRECISION
    pub liquidator_fee: u32,
    /// The fee the insurance fund receives from liquidation
    /// precision: LIQUIDATOR_FEE_PRECISION
    pub if_liquidation_fee: u32,
    /// The margin ratio which determines how much collateral is required to open a position
    /// e.g. margin ratio of .1 means a user must have $100 of total collateral to open a $1000 position
    /// precision: MARGIN_PRECISION
    pub margin_ratio_initial: u32,
    /// The margin ratio which determines when a user will be liquidated
    /// e.g. margin ratio of .05 means a user must have $50 of total collateral to maintain a $1000 position
    /// else they will be liquidated
    /// precision: MARGIN_PRECISION
    pub margin_ratio_maintenance: u32,
    /// The initial asset weight for positive pnl. Negative pnl always has an asset weight of 1
    /// precision: SPOT_WEIGHT_PRECISION
    pub unrealized_pnl_initial_asset_weight: u32,
    /// The maintenance asset weight for positive pnl. Negative pnl always has an asset weight of 1
    /// precision: SPOT_WEIGHT_PRECISION
    pub unrealized_pnl_maintenance_asset_weight: u32,
    /// number of users in a position (base)
    pub number_of_users_with_base: u32,
    /// number of users in a position (pnl) or pnl (quote)
    pub number_of_users: u32,
    pub market_index: u16,
    /// Whether a market is active, reduce only, expired, etc
    /// Affects whether users can open/close positions
    pub status: MarketStatus,
    /// Currently only Perpetual markets are supported
    pub contract_type: ContractType,
    /// The contract tier determines how much insurance a market can receive, with more speculative markets receiving less insurance
    /// It also influences the order perp markets can be liquidated, with less speculative markets being liquidated first
    pub contract_tier: ContractTier,
    pub paused_operations: u8,
    /// The spot market that pnl is settled in
    pub quote_spot_market_index: u16,
    /// Between -100 and 100, represents what % to increase/decrease the fee by
    /// E.g. if this is -50 and the fee is 5bps, the new fee will be 2.5bps
    /// if this is 50 and the fee is 5bps, the new fee will be 7.5bps
    pub fee_adjustment: i16,
    /// Number of unresolved bankrupt quote debts booked against this market.
    /// A liquidation that latches a user bankrupt increments it. Both writers
    /// of `PerpPosition.quote_asset_amount` decrement it when that debt
    /// reaches zero: `update_quote_asset_amount` and
    /// `update_position_and_market`. The count tracks the debt, not the latch:
    /// an un-latched estate that still owes the market stays booked, because
    /// the debt still resolves through the bankruptcy waterfall.
    ///
    /// While it is above zero the fee sweep withholds the whole
    /// `pending_if_fee`, not just `get_bankruptcy_if_floor()` — the sweep is
    /// permissionless, so a caller could otherwise drain the first-loss
    /// tranche between the latch and the resolution and push the loss onto the
    /// shared insurance fund or into socialization. The freeze is independent
    /// of open interest and of `bankruptcy_if_floor_pct`, both of which can be
    /// zero exactly when a bankruptcy is pending.
    ///
    /// Occupies 2 of the 6 bytes the Rust compiler inserts to 8-align
    /// `last_fill_price`. The remaining 4 stay explicit padding, so every
    /// later byte offset and the account size are unchanged and existing
    /// accounts read 0 (no pending claim).
    ///
    /// `settle_expired_market_pools_to_revenue_pool` rejects while this count
    /// is above zero, because that instruction's final sweep bypasses the
    /// floor.
    pub pending_bankruptcy_claims: u16,
    /// Explicit padding so the IDL records the 4 bytes the Rust compiler
    /// still inserts to 8-align `last_fill_price`. Without this the JS borsh
    /// decoder (which reads sequentially after the variable-span enum
    /// `status`) reads every field past `pending_bankruptcy_claims` 4 bytes
    /// early.
    pub _padding_align_lfp: [u8; 4],
    pub last_fill_price: u64,
    pub pool_id: u8,
    pub _padding_pmm: [u8; 2],
    /// Was `lp_fee_transfer_scalar`, `lp_status`, `lp_paused_operations`,
    /// `lp_exchange_fee_excluscion_scalar`, `lp_pool_id` (5×u8). Relocated into
    /// `hedge_config` at the tail; kept as reserved bytes so existing account
    /// byte offsets (and snapshots) are undisturbed.
    pub _padding_hedge: [u8; 5],
    pub market_config: u8,
    /// the oracle provider information. used to decode/scale the oracle public key
    pub oracle_source: OracleSource,
    /// Max oracle delay (legacy 400ms units) tolerated by immediate (JIT / auction-skipping)
    /// AMM fills. Positive is an explicit threshold. `0` disables immediate AMM
    /// fills entirely. Negative (the init default, `-1`) means unset, which
    /// resolves by price source: `MM_ORACLE_MIN_WRITE_GAP` for an MM-oracle-sourced
    /// price (the tightest window the crank can satisfy, since the program refuses
    /// MM-oracle writes closer together than that) and `0` for an exchange-oracle
    /// price, which can be same-slot fresh. See `math::oracle::oracle_validity`.
    pub oracle_slot_delay_override: i8,
    /// Low-risk oracle delay override (legacy 400ms units): 0 = unset (use the
    /// guard rail), otherwise a literal threshold. See `math::time::DelayOverride`.
    pub oracle_low_risk_slot_delay_override: i8,
    /// Floor on the unswept IF-fee carveout, as a percentage of open-interest
    /// notional (PERCENTAGE_PRECISION). The fee sweep's IF drain leaves
    /// `pending_if_fee` at (at least) this floor, so a standing first-loss
    /// tranche is available to `resolve_perp_bankruptcy` before any user is
    /// latched bankrupt — a permissionless sweep (or the inline sweep on any
    /// pnl settle) cannot drain the tranche below it. Notional is valued at
    /// the market's own oracle TWAP so a manipulated spot print can't crush
    /// the floor.
    ///
    /// `0` means `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT`, so every market created
    /// before the field existed carries the standing tranche without an admin
    /// call. `BANKRUPTCY_IF_FLOOR_DISABLED` turns the floor off. Read it
    /// through `get_bankruptcy_if_floor_pct`, never directly.
    ///
    /// The floor sizes the tranche off market risk, which is a proxy for the
    /// loss and can be smaller than it. `pending_bankruptcy_claims` covers
    /// every latched bankruptcy exactly, by withholding all of
    /// `pending_if_fee` until it resolves.
    ///
    /// Occupies the former 4-byte trailing padding before `market_stats`
    /// (same offset/alignment on all targets).
    pub bankruptcy_if_floor_pct: u32,
    /// Market-wide stats shared across all makers: mark/oracle TWAPs, std,
    /// volume, intensity, mm-oracle snapshot, `historical_oracle_data`,
    /// `last_oracle_normalised_price`, `last_oracle_valid`. Writers (e.g.
    /// `MarketStats::update_mark_std`, `update_volume_24h`, native
    /// `handle_update_mm_oracle_native`) update this directly.
    pub market_stats: MarketStats,
    /// Aggregate accrued builder/referrer revenue-share owed out of this
    /// market's `pnl_pool` but not yet paid: incremented as builder and
    /// referrer fees accrue on fills (mirrors the per-order
    /// `RevenueShareOrder.fees_accrued` writes) and decremented as
    /// `sweep_completed_revenue_share_for_market` pays them. The
    /// permissionless fee sweep reserves it (like `max(net_user_pnl, 0)` and
    /// the floored IF tranche) so a protocol-fee drain can't move the tokens
    /// backing already-owed revenue share out of the pnl pool and leave those
    /// claims temporarily unpayable. precision: QUOTE_PRECISION.
    ///
    /// Occupies the 8 bytes Rust naturally inserts to 16-align AMM's leading
    /// u128 (formerly explicit `_padding_align_amm`): a u64 at the same
    /// 8-aligned offset keeps every downstream byte offset and the total size
    /// unchanged, so legacy accounts read 0 (nothing owed) until fees accrue.
    pub pending_revenue_share: u64,
    /// The automated market maker. Last field so future quoter modules can
    /// land in the trailing region without disturbing earlier byte offsets
    /// — in the target architecture this account holds back-to-back
    /// per-quoter state slices (AMM, DLOB-maker state, future propAMM-style
    /// participants, …) and each module owns a contiguous span starting at
    /// a known offset.
    pub amm: AMM,
    /// This market's hedge (LP pool) configuration. Sits immediately after `amm`
    /// so the trailing `[amm, hedge_config]` span is the contiguous VLP region.
    pub hedge_config: HedgeConfig,
    /// This market's insurance fees that were swept from its pnl pool into the
    /// quote spot market revenue pool but have not yet reached the insurance
    /// fund vault. Bankruptcy for this market can reclaim the amount before
    /// drawing shared insurance capital. Other markets cannot consume it.
    /// precision: QUOTE_PRECISION
    pub insurance_fund_revenue_receivable: u64,
    /// Reserved tail space for future fields. Account extension is expensive
    /// operationally, so this upgrade allocates enough room for later additions.
    pub _padding_future: [u8; 248],
}

const _: () = assert!(std::mem::size_of::<PerpMarket>() == 1552);
const _: () = assert!(std::mem::offset_of!(PerpMarket, insurance_fund_revenue_receivable) == 1296);
const _: () = assert!(std::mem::offset_of!(PerpMarket, _padding_future) == 1304);

impl Default for PerpMarket {
    fn default() -> Self {
        PerpMarket {
            pubkey: Pubkey::default(),
            base_asset_amount_long: 0,
            base_asset_amount_short: 0,
            quote_asset_amount: 0,
            quote_entry_amount_long: 0,
            quote_entry_amount_short: 0,
            quote_break_even_amount_long: 0,
            quote_break_even_amount_short: 0,
            max_open_interest: 0,
            total_social_loss: 0,
            cumulative_funding_rate_long: 0,
            cumulative_funding_rate_short: 0,
            fee_ledger: FeeLedger::default(),
            oracle: Pubkey::default(),
            pnl_pool: PoolBalance::default(),
            name: [0; 32],
            insurance_claim: InsuranceClaim::default(),
            last_funding_rate: 0,
            last_funding_rate_long: 0,
            last_funding_rate_short: 0,
            last_funding_rate_ts: 0,
            net_unsettled_funding_pnl: 0,
            funding_clamp_threshold: 5,                   // 5bps
            funding_ramp_slope: PERCENTAGE_PRECISION_U32, // 1.0x
            order_step_size: 0,
            order_tick_size: 0,
            unrealized_pnl_max_imbalance: 0,
            expiry_ts: 0,
            expiry_price: 0,
            next_fill_record_id: 0,
            next_funding_rate_record_id: 0,
            imf_factor: 0,
            unrealized_pnl_imf_factor: 0,
            liquidator_fee: 0,
            if_liquidation_fee: 0,
            margin_ratio_initial: 0,
            margin_ratio_maintenance: 0,
            unrealized_pnl_initial_asset_weight: 0,
            unrealized_pnl_maintenance_asset_weight: 0,
            number_of_users_with_base: 0,
            number_of_users: 0,
            market_index: 0,
            status: MarketStatus::default(),
            contract_type: ContractType::default(),
            contract_tier: ContractTier::default(),
            paused_operations: 0,
            quote_spot_market_index: 0,
            fee_adjustment: 0,
            pending_bankruptcy_claims: 0,
            _padding_align_lfp: [0; 4],
            pool_id: 0,
            _padding_pmm: [0; 2],
            _padding_hedge: [0; 5],
            last_fill_price: 0,
            market_config: 0,
            oracle_source: OracleSource::default(),
            oracle_slot_delay_override: -1,
            oracle_low_risk_slot_delay_override: 0,
            bankruptcy_if_floor_pct: 0,
            market_stats: MarketStats::default(),
            pending_revenue_share: 0,
            amm: AMM::default(),
            hedge_config: HedgeConfig::default(),
            insurance_fund_revenue_receivable: 0,
            _padding_future: [0; 248],
            protocol_fee_pool: PoolBalance::default(),
            protocol_liquidation_fee: 0,
            taker_fee_addon_tenth_bps: 0,
            _padding_buffer: [0; 2],
            fee_pool_buffer_target: 0,
        }
    }
}

impl Size for PerpMarket {
    // 1200-byte struct + 8-byte discriminator. The cached spread state
    // (4×u128 spread reserves, i64 last_oracle_reserve_price_spread_pct,
    // 2×u32 long/short_spread, i32 reference_price_offset) plus a dedicated
    // u64 last_spread_update_slot live back on AMM — refreshed by
    // `math::spread::update_amm_quote_state` on each crank/fill `setup` and
    // read directly by quote/fill paths and dashboards.
    const SIZE: usize = 1560;
}

impl MarketIndexOffset for PerpMarket {
    // Account-byte offset (includes the 8-byte Anchor discriminator). Used
    // by callers that read `market_index` straight out of the account-bytes
    // slice without deserialising the full struct.
    const MARKET_INDEX_OFFSET: usize = 8 + std::mem::offset_of!(PerpMarket, market_index);
}

impl PerpMarket {
    pub fn oracle_id(&self) -> OracleIdentifier {
        (self.oracle, self.oracle_source)
    }

    pub fn has_market_config_flag(&self, flag: MarketConfigFlag) -> bool {
        self.market_config & flag as u8 != 0
    }

    pub fn is_in_settlement(&self, now: i64) -> bool {
        let in_settlement = matches!(
            self.status,
            MarketStatus::Settlement | MarketStatus::Delisted
        );
        let expired = self.expiry_ts != 0 && now >= self.expiry_ts;
        in_settlement || expired
    }

    pub fn is_reduce_only(&self) -> VelocityResult<bool> {
        Ok(self.status == MarketStatus::ReduceOnly)
    }

    pub fn is_operation_paused(&self, operation: PerpOperation) -> bool {
        PerpOperation::is_operation_paused(self.paused_operations, operation)
    }

    pub fn can_skip_auction_duration(
        &self,
        state: &State,
        amm_has_low_enough_inventory: bool,
    ) -> VelocityResult<bool> {
        // Honor both the exchange-wide immediate-fill breaker and the
        // market-scoped `AmmImmediateFill` pause bit; either being set forces
        // the auction to run its full duration instead of skipping to an
        // immediate AMM fill.
        if state.amm_immediate_fill_paused()?
            || self.is_operation_paused(PerpOperation::AmmImmediateFill)
        {
            return Ok(false);
        }

        let amm_low_inventory_and_profitable = self.amm.net_revenue_since_last_funding
            >= DEFAULT_REVENUE_SINCE_LAST_FUNDING_SPREAD_RETREAT
            && amm_has_low_enough_inventory;

        if amm_low_inventory_and_profitable {
            msg!("market {} amm skipping auction duration", self.market_index);
        }

        Ok(amm_low_inventory_and_profitable)
    }

    pub fn has_too_much_drawdown(&self) -> VelocityResult<bool> {
        self.amm.has_too_much_drawdown(self.contract_tier)
    }

    pub fn get_max_confidence_interval_multiplier(self) -> VelocityResult<u64> {
        // assuming validity_guard_rails max confidence pct is 2%
        Ok(match self.contract_tier {
            ContractTier::A => 1,                  // 2%
            ContractTier::B => 1,                  // 2%
            ContractTier::C => 2,                  // 4%
            ContractTier::Speculative => 10,       // 20%
            ContractTier::HighlySpeculative => 50, // 100%
            ContractTier::Isolated => 50,          // 100%
        })
    }

    /// PerpMarket-level oracle bookkeeping: refresh the oracle TWAPs,
    /// cache the latest reference-price-offset (used by the next quote's
    /// smoothing branch), and stamp `last_oracle_valid`. Called from the
    /// `update_amms` keeper crank and the bid-ask-twap keeper ix. This is a
    /// PerpMarket-side concern — it does NOT mutate AMM fields. It does read
    /// the AMM (for `reserve_price` and the spread snapshot used to derive
    /// the offset).
    ///
    /// Callers that then *gate* on a TWAP this would move must not use this
    /// composed form — see [`Self::refresh_amm_quote_state`].
    pub fn update_oracle_derived_stats(
        &mut self,
        mm_oracle_price_data: &crate::state::oracle::MMOraclePriceData,
        oracle_validity: Option<crate::math::oracle::OracleValidity>,
        now: i64,
        clock_slot: u64,
        slot_duration: SlotDuration,
    ) -> VelocityResult<()> {
        let Some(oracle_validity) = oracle_validity else {
            return Ok(());
        };

        let reserve_price_after = self.amm.reserve_price()?;

        self.refresh_oracle_twaps(
            mm_oracle_price_data,
            oracle_validity,
            now,
            reserve_price_after,
        )?;
        self.refresh_amm_quote_state_inner(
            mm_oracle_price_data,
            oracle_validity,
            clock_slot,
            reserve_price_after,
            slot_duration,
        )
    }

    /// Advance the funding-period and 5-minute oracle TWAPs, when the oracle is
    /// valid for `UpdateTwap`.
    ///
    /// Split out of [`Self::update_oracle_derived_stats`] so a caller that gates
    /// on the *pre-refresh* TWAP can skip it. Refreshing first would drag the
    /// TWAP toward the live price and let a too-volatile / too-divergent oracle
    /// clear its own gate inside the same instruction (OtterSec #109).
    fn refresh_oracle_twaps(
        &mut self,
        mm_oracle_price_data: &crate::state::oracle::MMOraclePriceData,
        oracle_validity: crate::math::oracle::OracleValidity,
        now: i64,
        reserve_price: u64,
    ) -> VelocityResult<()> {
        if crate::math::oracle::is_oracle_valid_for_action(
            oracle_validity,
            Some(crate::math::oracle::VelocityAction::UpdateTwap),
        )? {
            let sanitize_clamp_denominator = self.get_sanitize_clamp_denominator()?;
            let PerpMarket {
                amm, market_stats, ..
            } = self;
            market_stats.update_oracle_twap(
                amm,
                now,
                mm_oracle_price_data,
                Some(reserve_price),
                sanitize_clamp_denominator,
            )?;
        }

        Ok(())
    }

    /// Refresh the AMM's cached quote state and stamp `last_oracle_valid`,
    /// **without** touching the oracle TWAPs.
    ///
    /// This is the half of [`Self::update_oracle_derived_stats`] that is safe to
    /// run ahead of a check that reads `last_oracle_price_twap` /
    /// `last_oracle_price_twap_5min`.
    pub fn refresh_amm_quote_state(
        &mut self,
        mm_oracle_price_data: &crate::state::oracle::MMOraclePriceData,
        oracle_validity: Option<crate::math::oracle::OracleValidity>,
        clock_slot: u64,
        slot_duration: SlotDuration,
    ) -> VelocityResult<()> {
        let Some(oracle_validity) = oracle_validity else {
            return Ok(());
        };

        let reserve_price_after = self.amm.reserve_price()?;
        self.refresh_amm_quote_state_inner(
            mm_oracle_price_data,
            oracle_validity,
            clock_slot,
            reserve_price_after,
            slot_duration,
        )
    }

    fn refresh_amm_quote_state_inner(
        &mut self,
        mm_oracle_price_data: &crate::state::oracle::MMOraclePriceData,
        oracle_validity: crate::math::oracle::OracleValidity,
        clock_slot: u64,
        reserve_price: u64,
        slot_duration: SlotDuration,
    ) -> VelocityResult<()> {
        // Refresh the AMM's cached spread state (long/short spread, reference
        // offset, oracle-reserve spread pct, ask/bid reserves) in place, then
        // mirror the fresh reference offset into market_stats so the next
        // refresh can smooth-transition off it.
        let PerpMarket {
            amm, market_stats, ..
        } = self;
        crate::vlp::amm::math::spread::update_amm_quote_state(
            amm,
            market_stats,
            mm_oracle_price_data,
            reserve_price,
            clock_slot,
            slot_duration,
        )?;
        market_stats.last_reference_price_offset = amm.reference_price_offset;

        market_stats.last_oracle_valid = crate::math::oracle::is_oracle_valid_for_action(
            oracle_validity,
            Some(crate::math::oracle::VelocityAction::FillOrderAmmLowRisk),
        )?;

        Ok(())
    }

    pub fn get_sanitize_clamp_denominator(self) -> VelocityResult<Option<i64>> {
        Ok(match self.contract_tier {
            ContractTier::A => Some(10_i64),         // 10%
            ContractTier::B => Some(5_i64),          // 20%
            ContractTier::C => Some(2_i64),          // 50%
            ContractTier::Speculative => None, // DEFAULT_MAX_TWAP_UPDATE_PRICE_BAND_DENOMINATOR
            ContractTier::HighlySpeculative => None, // DEFAULT_MAX_TWAP_UPDATE_PRICE_BAND_DENOMINATOR
            ContractTier::Isolated => None, // DEFAULT_MAX_TWAP_UPDATE_PRICE_BAND_DENOMINATOR
        })
    }

    pub fn get_auction_end_min_max_divisors(self) -> VelocityResult<(u64, u64)> {
        Ok(match self.contract_tier {
            ContractTier::A => (1000, 50),              // 10 bps, 2%
            ContractTier::B => (1000, 20),              // 10 bps, 5%
            ContractTier::C => (500, 20),               // 50 bps, 5%
            ContractTier::Speculative => (100, 10),     // 1%, 10%
            ContractTier::HighlySpeculative => (50, 5), // 2%, 20%
            ContractTier::Isolated => (50, 5),          // 2%, 20%
        })
    }

    pub fn get_max_price_divergence_for_funding_rate(
        self,
        oracle_price_twap: i64,
    ) -> VelocityResult<i64> {
        // clamp to 3% price divergence for safer markets and higher for lower contract tiers
        if self.contract_tier.is_as_safe_as_contract(&ContractTier::B) {
            oracle_price_twap.safe_div(33) // 3%
        } else if self.contract_tier.is_as_safe_as_contract(&ContractTier::C) {
            oracle_price_twap.safe_div(20) // 5%
        } else {
            oracle_price_twap.safe_div(10) // 10%
        }
    }

    pub fn get_margin_ratio(
        &self,
        size: u128,
        margin_type: MarginRequirementType,
    ) -> VelocityResult<u32> {
        if self.status == MarketStatus::Settlement {
            return Ok(0);
        }

        let default_margin_ratio = match margin_type {
            MarginRequirementType::Initial => self.margin_ratio_initial,
            MarginRequirementType::Fill => {
                self.margin_ratio_initial
                    .safe_add(self.margin_ratio_maintenance)?
                    / 2
            }
            MarginRequirementType::Maintenance => self.margin_ratio_maintenance,
        };

        let size_adj_margin_ratio = calculate_size_premium_liability_weight(
            size,
            self.imf_factor,
            default_margin_ratio,
            MARGIN_PRECISION_U128,
            true,
        )?;

        let margin_ratio = default_margin_ratio.max(size_adj_margin_ratio);

        Ok(margin_ratio)
    }

    pub fn get_base_liquidator_fee(&self) -> u32 {
        self.liquidator_fee
    }

    pub fn get_max_liquidation_fee(&self) -> VelocityResult<u32> {
        let max_liquidation_fee = (self.liquidator_fee.safe_mul(MAX_LIQUIDATION_MULTIPLIER)?).min(
            self.margin_ratio_maintenance
                .safe_mul(LIQUIDATION_FEE_PRECISION / MARGIN_PRECISION)
                .unwrap_or(u32::MAX),
        );
        Ok(max_liquidation_fee)
    }

    pub fn get_unrealized_asset_weight(
        &self,
        unrealized_pnl: i128,
        margin_type: MarginRequirementType,
    ) -> VelocityResult<u32> {
        let mut margin_asset_weight = match margin_type {
            MarginRequirementType::Initial | MarginRequirementType::Fill => {
                self.unrealized_pnl_initial_asset_weight
            }
            MarginRequirementType::Maintenance => self.unrealized_pnl_maintenance_asset_weight,
        };

        if margin_asset_weight > 0
            && matches!(
                margin_type,
                MarginRequirementType::Fill | MarginRequirementType::Initial
            )
            && self.unrealized_pnl_max_imbalance > 0
        {
            let net_unsettled_pnl = amm::calculate_net_user_pnl(
                &self.amm,
                self.market_stats.historical_oracle_data.last_oracle_price,
                self.quote_asset_amount,
                self.net_unsettled_funding_pnl,
            )?;

            if net_unsettled_pnl > self.unrealized_pnl_max_imbalance.cast::<i128>()? {
                margin_asset_weight = margin_asset_weight
                    .cast::<u128>()?
                    .safe_mul(self.unrealized_pnl_max_imbalance.cast()?)?
                    .safe_div(net_unsettled_pnl.unsigned_abs())?
                    .cast()?;
            }
        }

        // the asset weight for a position's unrealized pnl + unsettled pnl in the margin system
        // > 0 (positive balance)
        // < 0 (negative balance) always has asset weight = 1
        let unrealized_asset_weight = if unrealized_pnl > 0 {
            // todo: only discount the initial margin s.t. no one gets liquidated over upnl?

            // a larger imf factor -> lower asset weight
            match margin_type {
                MarginRequirementType::Initial | MarginRequirementType::Fill => {
                    if margin_asset_weight > 0 {
                        calculate_size_discount_asset_weight(
                            unrealized_pnl
                                .unsigned_abs()
                                .safe_mul(AMM_TO_QUOTE_PRECISION_RATIO)?,
                            self.unrealized_pnl_imf_factor,
                            margin_asset_weight,
                        )?
                    } else {
                        0
                    }
                }
                MarginRequirementType::Maintenance => self.unrealized_pnl_maintenance_asset_weight,
            }
        } else {
            SPOT_WEIGHT_PRECISION
        };

        Ok(unrealized_asset_weight)
    }

    pub fn get_open_interest(&self) -> u128 {
        self.base_asset_amount_long
            .abs()
            .max(self.base_asset_amount_short.abs())
            .unsigned_abs()
    }

    /// The effective floor percentage. `0` is the value every market written
    /// before the field existed holds, so it resolves to
    /// `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT` — a market gets the standing tranche
    /// without an admin call. `BANKRUPTCY_IF_FLOOR_DISABLED` resolves to 0.
    /// precision: PERCENTAGE_PRECISION
    pub fn get_bankruptcy_if_floor_pct(&self) -> u32 {
        match self.bankruptcy_if_floor_pct {
            0 => DEFAULT_BANKRUPTCY_IF_FLOOR_PCT,
            BANKRUPTCY_IF_FLOOR_DISABLED => 0,
            pct => pct,
        }
    }

    /// True while at least one latched bankruptcy still holds an unresolved
    /// quote debt against this market.
    pub fn has_pending_bankruptcy_claim(&self) -> bool {
        self.pending_bankruptcy_claims > 0
    }

    /// Book a latched bankrupt debt against this market. The fee sweep then
    /// withholds the whole `pending_if_fee` until the debt resolves.
    pub fn increment_pending_bankruptcy_claims(&mut self) {
        self.pending_bankruptcy_claims = self.pending_bankruptcy_claims.saturating_add(1);
    }

    /// Discharge a booked debt. Saturating: an extra decrement must not wrap
    /// the counter to a value that freezes the sweep forever.
    pub fn decrement_pending_bankruptcy_claims(&mut self) {
        self.pending_bankruptcy_claims = self.pending_bankruptcy_claims.saturating_sub(1);
    }

    /// The `pending_if_fee` floor the sweep's IF drain must leave behind:
    /// `get_bankruptcy_if_floor_pct()` of open-interest notional, valued at
    /// the market's oracle TWAP (manipulation-resistant; no live oracle
    /// needed). This is the standing tranche, held before any user is latched.
    /// precision: QUOTE_PRECISION
    pub fn get_bankruptcy_if_floor(&self) -> VelocityResult<u128> {
        let bankruptcy_if_floor_pct = self.get_bankruptcy_if_floor_pct();
        if bankruptcy_if_floor_pct == 0 {
            return Ok(0);
        }

        let oracle_price_twap = self
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap
            .max(0)
            .cast::<u128>()?;

        self.get_open_interest()
            .safe_mul(oracle_price_twap)?
            .safe_div(BASE_PRECISION)?
            .safe_mul(bankruptcy_if_floor_pct.cast()?)?
            .safe_div(PERCENTAGE_PRECISION)
    }

    /// The `pending_if_fee` the sweep's IF drain must leave behind. A latched
    /// bankruptcy holds all of it; otherwise the standing floor holds its
    /// part. `force` (the delisting sweep) holds nothing, because the delist
    /// handler rejects while `pending_bankruptcy_claims` is above zero.
    /// precision: QUOTE_PRECISION
    pub fn get_pending_if_fee_floor(&self, force: bool) -> VelocityResult<u128> {
        if force {
            return Ok(0);
        }

        if self.has_pending_bankruptcy_claim() {
            return Ok(self.fee_ledger.pending_if_fee);
        }

        self.get_bankruptcy_if_floor()
    }

    /// The pnl-pool tokens that must stay behind to keep the first-loss IF
    /// bankruptcy tranche backed: `min(pending_if_fee,
    /// get_pending_if_fee_floor())`. `resolve_perp_bankruptcy` consumes
    /// `pending_if_fee` counter-only (it cancels the forgiven loss against the
    /// pending claim with no token movement, relying on that fee value still
    /// sitting in the pnl pool), so a permissionless drain that moved those
    /// tokens elsewhere — e.g. `sweep_market_fees`' buffer-exempt protocol cut
    /// into `protocol_fee_pool`, which is not part of the insurance backstop —
    /// would leave the tranche unbacked and surviving-trader PnL short. Every
    /// permissionless pnl-pool drain reserves this on top of
    /// `max(net_user_pnl, 0)`. `force` (delisting, which the handler allows
    /// only after every bankruptcy claim is discharged) returns 0.
    /// precision: QUOTE_PRECISION
    pub fn get_bankruptcy_if_tranche_reservation(&self, force: bool) -> VelocityResult<u128> {
        Ok(self
            .fee_ledger
            .pending_if_fee
            .min(self.get_pending_if_fee_floor(force)?))
    }

    /// Record builder/referrer revenue share accrued on a fill into the
    /// per-market aggregate (mirrors the per-order `fees_accrued` write so the
    /// fee sweep can reserve the tokens backing it).
    pub fn accrue_pending_revenue_share(&mut self, amount: u64) -> VelocityResult {
        self.pending_revenue_share = self.pending_revenue_share.safe_add(amount)?;
        Ok(())
    }

    /// Discharge revenue share from the per-market aggregate as
    /// `sweep_completed_revenue_share_for_market` pays it out of the pnl pool.
    pub fn settle_pending_revenue_share(&mut self, amount: u64) -> VelocityResult {
        self.pending_revenue_share = self.pending_revenue_share.saturating_sub(amount);
        Ok(())
    }

    pub fn get_market_depth_for_funding_rate(&self) -> VelocityResult<u64> {
        // base amount used on user orders for funding calculation

        let open_interest = self.get_open_interest();

        let depth = (open_interest.safe_div(1000)?.cast::<u64>()?).clamp(
            self.market_stats.min_order_size.safe_mul(100)?,
            self.market_stats.min_order_size.safe_mul(5000)?,
        );

        Ok(depth)
    }

    pub fn is_price_divergence_ok_for_settle_pnl(&self, oracle_price: i64) -> VelocityResult<bool> {
        let oracle_divergence = oracle_price
            .safe_sub(
                self.market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
            )?
            .safe_mul(PERCENTAGE_PRECISION_I64)?
            .safe_div(
                self.market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap_5min
                    .min(oracle_price),
            )?
            .unsigned_abs();

        let oracle_divergence_limit = match self.contract_tier {
            ContractTier::A => PERCENTAGE_PRECISION_U64 / 200, // 50 bps
            ContractTier::B => PERCENTAGE_PRECISION_U64 / 200, // 50 bps
            ContractTier::C => PERCENTAGE_PRECISION_U64 / 100, // 100 bps
            ContractTier::Speculative => PERCENTAGE_PRECISION_U64 / 40, // 250 bps
            ContractTier::HighlySpeculative => PERCENTAGE_PRECISION_U64 / 40, // 250 bps
            ContractTier::Isolated => PERCENTAGE_PRECISION_U64 / 40, // 250 bps
        };

        if oracle_divergence >= oracle_divergence_limit {
            msg!(
                "market_index={} price divergence too large to safely settle pnl: {} >= {}",
                self.market_index,
                oracle_divergence,
                oracle_divergence_limit
            );
            return Ok(false);
        }

        let min_price = oracle_price.min(
            self.market_stats
                .historical_oracle_data
                .last_oracle_price_twap_5min,
        );

        let std_limit = match self.contract_tier {
            ContractTier::A => min_price / 50,                 // 200 bps
            ContractTier::B => min_price / 50,                 // 200 bps
            ContractTier::C => min_price / 20,                 // 500 bps
            ContractTier::Speculative => min_price / 10,       // 1000 bps
            ContractTier::HighlySpeculative => min_price / 10, // 1000 bps
            ContractTier::Isolated => min_price / 10,          // 1000 bps
        }
        .unsigned_abs();

        if self.market_stats.oracle_std.max(self.market_stats.mark_std) >= std_limit {
            msg!(
                "market_index={} std too large to safely settle pnl: {} >= {}",
                self.market_index,
                self.market_stats.oracle_std.max(self.market_stats.mark_std),
                std_limit
            );
            return Ok(false);
        }

        Ok(true)
    }

    pub fn can_sanitize_market_order_auctions(&self) -> bool {
        self.oracle_source != OracleSource::Prelaunch
    }

    /// Reference price for evaluating trigger (TP/SL) orders:
    ///
    /// `trigger_price = clamp(median(leg_a, leg_b, leg_c))`
    ///
    /// - Leg A: `last_fill_price`, the last trade on this market. The oracle
    ///   price substitutes until the market's first fill (`last_fill_price == 0`)
    ///   and when the last fill is stale (older than
    ///   `TRIGGER_PRICE_LAST_FILL_MAX_AGE`).
    /// - Leg B: `oracle + funding_basis`, the price premium implied by the last
    ///   funding rate, decaying linearly with its age (`get_last_funding_basis`).
    /// - Leg C: `oracle + basis_5min`, where
    ///   `basis_5min = mark_twap_5min - oracle_twap_5min`.
    ///
    /// The median is clamped to a per-tier band around the oracle price
    /// (`clamp_trigger_price`). With `use_median_price` off, returns the raw
    /// oracle price.
    pub fn get_trigger_price(
        &self,
        oracle_price: i64,
        now: i64,
        use_median_price: bool,
    ) -> VelocityResult<u64> {
        if !use_median_price {
            return oracle_price.cast::<u64>();
        }

        // Leg A: last trade price, only while fresh. `last_trade_ts` is
        // stamped by the same fill path that writes `last_fill_price`.
        let last_fill_price = self.last_fill_price;
        let last_fill_is_fresh =
            now.safe_sub(self.market_stats.last_trade_ts)? <= TRIGGER_PRICE_LAST_FILL_MAX_AGE;

        // Leg C: oracle + (mark_twap_5min - oracle_twap_5min)
        let mark_price_5min_twap = self.market_stats.last_mark_price_twap_5min;
        let last_oracle_price_twap_5min = self
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_5min;

        let basis_5min = mark_price_5min_twap
            .cast::<i64>()?
            .safe_sub(last_oracle_price_twap_5min)?;

        let oracle_plus_basis_5min = oracle_price.safe_add(basis_5min)?.cast::<u64>()?;

        // Leg B: oracle + decayed funding basis
        let last_funding_basis = self.get_last_funding_basis(oracle_price, now)?;

        let oracle_plus_funding_basis = oracle_price.safe_add(last_funding_basis)?.cast::<u64>()?;

        let median_price = if last_fill_price > 0 && last_fill_is_fresh {
            msg!(
                "last_fill_price: {} oracle_plus_funding_basis: {} oracle_plus_basis_5min: {}",
                last_fill_price,
                oracle_plus_funding_basis,
                oracle_plus_basis_5min
            );
            let mut prices = [
                last_fill_price,
                oracle_plus_funding_basis,
                oracle_plus_basis_5min,
            ];
            prices.sort_unstable();

            prices[1]
        } else {
            // No fill yet or last fill is stale: oracle price stands in for Leg A
            let mut prices = [
                oracle_price.unsigned_abs(),
                oracle_plus_funding_basis,
                oracle_plus_basis_5min,
            ];
            prices.sort_unstable();

            prices[1]
        };

        self.clamp_trigger_price(oracle_price.unsigned_abs(), median_price)
    }

    /// Price basis implied by the last funding rate (Leg B of the trigger price).
    ///
    /// ```text
    /// daily_rate = last_funding_rate / last_funding_oracle_twap * 24 - funding_rate_offset
    /// basis      = oracle_price * daily_rate * (funding_period - time_since_funding_update) / funding_period
    /// ```
    ///
    /// A fresh funding print contributes the full basis; it decays linearly to
    /// zero as it ages toward one funding period. Returns 0 with no funding
    /// history (`last_funding_oracle_twap <= 0`).
    #[inline(always)]
    fn get_last_funding_basis(&self, oracle_price: i64, now: i64) -> VelocityResult<i64> {
        if self.market_stats.last_funding_oracle_twap > 0 {
            let last_funding_rate = self
                .last_funding_rate
                .cast::<i128>()?
                .safe_mul(PRICE_PRECISION_I128)?
                .safe_div(self.market_stats.last_funding_oracle_twap.cast::<i128>()?)?
                .safe_mul(24)?;
            let last_funding_rate_pre_adj =
                last_funding_rate.safe_sub(FUNDING_RATE_OFFSET_PERCENTAGE as i128)?;

            let funding_period = self.market_stats.funding_period;
            let time_since_funding_update =
                now.safe_sub(self.last_funding_rate_ts)?.min(funding_period);

            let last_funding_basis = oracle_price
                .cast::<i128>()?
                .safe_mul(last_funding_rate_pre_adj)?
                .safe_div(PERCENTAGE_PRECISION_I128)?
                .safe_mul(
                    funding_period
                        .safe_sub(time_since_funding_update)?
                        .cast::<i128>()?,
                )?
                .safe_div(funding_period.cast::<i128>()?)?
                / FUNDING_RATE_BUFFER_I128;

            last_funding_basis.cast::<i64>()
        } else {
            Ok(0)
        }
    }

    /// Clamps the median trigger price to a band around the oracle price.
    /// Band width by contract tier: A/B 20 bps, C 100 bps, rest 250 bps.
    #[inline(always)]
    fn clamp_trigger_price(&self, oracle_price: u64, median_price: u64) -> VelocityResult<u64> {
        let clamp_divisor = if matches!(self.contract_tier, ContractTier::A | ContractTier::B) {
            500 // oracle / 500 = 20 bps
        } else if matches!(self.contract_tier, ContractTier::C) {
            100 // oracle / 100 = 100 bps
        } else {
            40 // oracle / 40 = 250 bps
        };
        let max_oracle_diff = oracle_price / clamp_divisor;

        Ok(median_price.clamp(
            oracle_price.safe_sub(max_oracle_diff)?,
            oracle_price.safe_add(max_oracle_diff)?,
        ))
    }

    /// Whether `oracle_price_data` is the same sample (price, confidence) the
    /// last oracle-stat update stamped into `historical_oracle_data`. An
    /// oracle write landing after the AMM update within the same slot
    /// replaces the account's price and confidence without touching
    /// `last_oracle_valid`, so a cached validity verdict must not be applied
    /// to a sample it never covered. A rewrite that leaves price and
    /// confidence unchanged can only reduce delay, which cannot make the
    /// sample less valid.
    pub fn is_validated_oracle_sample(&self, oracle_price_data: &OraclePriceData) -> bool {
        let historical = &self.market_stats.historical_oracle_data;
        oracle_price_data.price == historical.last_oracle_price
            && oracle_price_data.confidence == historical.last_oracle_conf
    }

    /// Whether the oracle was valid at the last AMM update, the AMM was
    /// updated in the current slot, AND `oracle_price_data` is the sample
    /// that update validated. Slot freshness alone is not a substitute for
    /// current validity: a second oracle write in the same slot can replace
    /// the sample after the AMM update.
    pub fn is_recent_oracle_valid(
        &self,
        current_slot: u64,
        oracle_price_data: &OraclePriceData,
    ) -> VelocityResult<bool> {
        Ok(self.market_stats.last_oracle_valid
            && self.amm.is_fresh_at(current_slot)
            && self.is_validated_oracle_sample(oracle_price_data))
    }

    #[inline(always)]
    pub fn get_mm_oracle_price_data(
        &self,
        oracle_price_data: OraclePriceData,
        clock_slot: u64,
        oracle_guard_rails: &ValidityGuardRails,
        slot_duration: SlotDuration,
    ) -> VelocityResult<MMOraclePriceData> {
        let delay = clock_slot
            .cast::<i64>()?
            .safe_sub(self.market_stats.mm_oracle_slot.cast::<i64>()?)?;
        let oracle_data = OraclePriceData {
            price: self.market_stats.mm_oracle_price,
            delay,
            sequence_id: None,
            confidence: oracle_price_data.confidence,
            has_sufficient_number_of_data_points: true,
        };
        let oracle_validity = if self.market_stats.mm_oracle_price == 0 {
            OracleValidity::NonPositive
        } else {
            oracle_validity(
                MarketType::Perp,
                self.market_index,
                self.market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap,
                &oracle_data,
                oracle_guard_rails,
                self.get_max_confidence_interval_multiplier()?,
                &self.oracle_source,
                LogMode::MMOracle,
                self.oracle_slot_delay_override,
                true, // classifying the MM oracle price itself
                self.oracle_low_risk_slot_delay_override,
                slot_duration,
            )?
        };
        MMOraclePriceData::new(
            self.market_stats.mm_oracle_price,
            delay,
            self.market_stats.mm_oracle_sequence_id,
            oracle_validity,
            oracle_price_data,
        )
    }

    /// Hard gates suppressing all AMM fills (standalone and JIT): `AmmFill`
    /// pause, drawdown, MM-oracle volatility, oracle validity. Global
    /// `amm_paused()` is the caller's. Excludes the auction-timing gates in
    /// [`Self::amm_can_fill_order`], which JIT bypasses by design.
    pub fn amm_fill_gates_ok(
        &self,
        safe_oracle_validity: OracleValidity,
        mm_oracle_price_data: &MMOraclePriceData,
    ) -> VelocityResult<bool> {
        if self.is_operation_paused(PerpOperation::AmmFill) {
            msg!("AMM cannot fill order: AMM fill operation is paused");
            return Ok(false);
        }

        if self.has_too_much_drawdown()? {
            msg!("AMM cannot fill order: has too much drawdown");
            return Ok(false);
        }

        // We are already using safe oracle data from MM oracle.
        // But AMM isnt available if we could have used MM oracle but fell back due to price diff
        // This is basically early volatility protection
        let mm_oracle_not_too_volatile =
            if mm_oracle_price_data.is_enabled() && mm_oracle_price_data.is_mm_oracle_as_recent() {
                !mm_oracle_price_data.is_mm_exchange_diff_bps_high()
            } else {
                true
            };

        if !mm_oracle_not_too_volatile {
            msg!("AMM cannot fill order: MM oracle too volatile compared to exchange oracle");
            return Ok(false);
        }

        // Determine if order is fillable with low risk
        let oracle_valid_for_amm_fill_low_risk = is_oracle_valid_for_action(
            safe_oracle_validity,
            Some(VelocityAction::FillOrderAmmLowRisk),
        )?;
        if !oracle_valid_for_amm_fill_low_risk {
            msg!("AMM cannot fill order: oracle not valid for low risk fills");
            return Ok(false);
        }

        Ok(true)
    }

    pub fn amm_can_fill_order(
        &self,
        order: &Order,
        clock_slot: u64,
        fill_mode: FillMode,
        state: &State,
        safe_oracle_validity: OracleValidity,
        user_can_skip_auction_duration: bool,
        mm_oracle_price_data: &MMOraclePriceData,
    ) -> VelocityResult<bool> {
        Ok(
            self.amm_fill_gates_ok(safe_oracle_validity, mm_oracle_price_data)?
                && self.amm_fill_timing_ok(
                    order,
                    clock_slot,
                    fill_mode,
                    state,
                    safe_oracle_validity,
                    user_can_skip_auction_duration,
                    mm_oracle_price_data,
                )?,
        )
    }

    /// Auction-timing / order-risk half of [`Self::amm_can_fill_order`], run
    /// after [`Self::amm_fill_gates_ok`]. A low-risk order fills; otherwise the
    /// AMM only fills immediately (JIT) when it wants to make, has room, and
    /// can skip the auction. JIT inside a DLOB match bypasses this by design.
    fn amm_fill_timing_ok(
        &self,
        order: &Order,
        clock_slot: u64,
        fill_mode: FillMode,
        state: &State,
        safe_oracle_validity: OracleValidity,
        user_can_skip_auction_duration: bool,
        mm_oracle_price_data: &MMOraclePriceData,
    ) -> VelocityResult<bool> {
        let safe_oracle_price_data = mm_oracle_price_data.get_safe_oracle_price_data();
        let can_fill_low_risk = order.is_low_risk_for_amm(
            safe_oracle_price_data.delay,
            clock_slot,
            fill_mode.is_liquidation(),
            user_can_skip_auction_duration,
        )?;
        if can_fill_low_risk {
            return Ok(true);
        }

        // Higher-risk order: only fillable immediately (JIT).
        if !user_can_skip_auction_duration {
            msg!("AMM cannot fill order: user has paused operations");
            return Ok(false);
        }

        let oracle_valid_for_can_fill_immediately = is_oracle_valid_for_action(
            safe_oracle_validity,
            Some(VelocityAction::FillOrderAmmImmediate),
        )?;
        if !oracle_valid_for_can_fill_immediately {
            msg!("AMM cannot fill order: oracle not valid for immediate fills");
            return Ok(false);
        }

        let amm_wants_to_jit_make = self
            .amm
            .amm_wants_to_jit_make(order.direction, self.order_step_size)?;
        if !amm_wants_to_jit_make {
            msg!("AMM cannot fill order: AMM does not want to JIT make");
            return Ok(false);
        }

        let amm_has_low_enough_inventory = self
            .amm
            .amm_has_low_enough_inventory(amm_wants_to_jit_make)?;
        if !amm_has_low_enough_inventory {
            msg!("AMM cannot fill order: AMM has too much inventory");
            return Ok(false);
        }

        let amm_can_skip_duration =
            self.can_skip_auction_duration(state, amm_has_low_enough_inventory)?;
        if !amm_can_skip_duration {
            msg!("AMM cannot fill order: AMM cannot skip duration");
            return Ok(false);
        }

        Ok(true)
    }
}

#[cfg(test)]
impl PerpMarket {
    pub fn default_test() -> Self {
        use crate::math::constants::PRICE_PRECISION_I64;
        let amm = AMM::default_test();
        PerpMarket {
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: PRICE_PRECISION_I64,
                    ..HistoricalOracleData::default()
                },
                last_oracle_valid: true,
                ..MarketStats::default()
            },
            amm,
            order_step_size: 1,
            order_tick_size: 1,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            ..PerpMarket::default()
        }
    }

    pub fn default_btc_test() -> Self {
        use crate::math::constants::{PRICE_PRECISION, PRICE_PRECISION_I64};
        let amm = AMM::default_btc_test();
        PerpMarket {
            market_stats: MarketStats {
                historical_oracle_data: HistoricalOracleData {
                    last_oracle_price: 19_400 * PRICE_PRECISION_I64,
                    last_oracle_price_twap: 19_400 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_5min: 19_400 * PRICE_PRECISION_I64,
                    last_oracle_price_twap_ts: 1_662_800_000_i64,
                    ..HistoricalOracleData::default()
                },
                last_mark_price_twap_ts: 1_662_800_000,
                mark_std: PRICE_PRECISION as u64,
                last_oracle_valid: true,
                funding_period: 3600,
                ..MarketStats::default()
            },
            amm,
            quote_asset_amount: 19_000_000_000, // short 1 BTC @ $19000
            margin_ratio_initial: 1000,         // 10x
            margin_ratio_maintenance: 500,      // 5x
            status: MarketStatus::Initialized,
            ..PerpMarket::default()
        }
    }
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct InsuranceClaim {
    /// The amount of revenue last settled
    /// Positive if funds left the perp market,
    /// negative if funds were pulled into the perp market
    /// precision: QUOTE_PRECISION
    pub revenue_withdraw_since_last_settle: i64,
    /// The max amount of revenue that can be withdrawn per period
    /// precision: QUOTE_PRECISION
    pub max_revenue_withdraw_per_period: u64,
    /// The max amount of insurance that perp market can use to resolve bankruptcy and pnl deficits
    /// precision: QUOTE_PRECISION
    pub quote_max_insurance: u64,
    /// The amount of insurance that has been used to resolve bankruptcy and pnl deficits
    /// precision: QUOTE_PRECISION
    pub quote_settled_insurance: u64,
    /// The last time revenue was settled in/out of market
    pub last_revenue_withdraw_ts: i64,
}

impl InsuranceClaim {
    /// Reset the per-period revenue-withdraw counter when the quote spot market
    /// has opened a new revenue-settle period since this market last withdrew.
    /// Both the fee sweep and the pnl-deficit path call this so they share one
    /// definition of "a new period has started" and never disagree on the cap.
    pub fn reset_revenue_withdraw_for_new_period(
        &mut self,
        spot_last_revenue_settle_ts: i64,
        now: i64,
    ) -> VelocityResult {
        if spot_last_revenue_settle_ts > self.last_revenue_withdraw_ts {
            validate!(
                now >= self.last_revenue_withdraw_ts && now >= spot_last_revenue_settle_ts,
                ErrorCode::BlockchainClockInconsistency,
                "issue with clock unix timestamp {} < market.insurance_claim.last_revenue_withdraw_ts={}/spot_market.last_revenue_settle_ts={}",
                now,
                self.last_revenue_withdraw_ts,
                spot_last_revenue_settle_ts,
            )?;
            self.revenue_withdraw_since_last_settle = 0;
        }
        Ok(())
    }
}

#[zero_copy(unsafe)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct PoolBalance {
    /// To get the pool's token amount, you must multiply the scaled balance by the market's cumulative
    /// deposit interest
    /// precision: SPOT_BALANCE_PRECISION
    pub scaled_balance: u128,
    /// The spot market the pool is for
    pub market_index: u16,
    /// Filler for the alignment gap before the two dust fields. Those fields must
    /// start at offsets 20 and 24. The host layout and the SBF layout then agree,
    /// and the packed borsh layout in the IDL reaches the same offsets. This
    /// field shrank from 14 bytes to 2. The size of the struct and every other
    /// field offset are unchanged. Do not reorder or resize these fields.
    pub padding: [u8; 2],
    /// Remainder of one index-space division that splits a spot market's deposit
    /// interest between lenders and the carveout pools. The accrual carries the
    /// remainder between intervals. A share too small to reach a whole index unit
    /// is therefore delayed and not lost.
    ///
    /// The division depends on the pool. See `split_deposit_interest`.
    ///
    ///   - On `revenue_pool` this is the lenders-vs-carveouts split. The divisor
    ///     is IF_FACTOR_PRECISION, so the value stays below IF_FACTOR_PRECISION.
    ///   - On `protocol_fee_pool` this is the insurance-fund-vs-protocol split of
    ///     the withheld amount. The divisor is
    ///     `if_fee_factor + protocol_fee_factor`, so the value stays below it. The
    ///     admin can lower that pair, which leaves a stored value at or above the
    ///     new divisor. `split_deposit_interest` reduces the value it reads below
    ///     the divisor in force, so a change of the factors costs less than one
    ///     index unit and cannot strand the market.
    ///
    /// That order keeps the two carveouts from taking more than the interval
    /// gain. The first division bounds the total. The second division only
    /// divides the amount that the first division set aside. Two independent cuts
    /// can instead each round up and leave lenders at zero.
    ///
    /// precision: the numerator units of its division.
    pub pending_interest_split_dust: u32,
    /// Remainder of the token-space division for this pool's carveout.
    ///
    /// A withheld index amount reaches the pool only as whole tokens, through
    /// `deposit_balance * cut / 10^(19 - decimals)`. On a small market that
    /// division floors to zero even when the index-space cut is not zero. Lenders
    /// have already given up the value at that point, so a floored cut credits
    /// nobody and leaves unattributed slack in the vault. The accrual parks the
    /// remainder here and adds it back on the next interval.
    ///
    /// precision: token * 10^(19 - decimals). The value always stays below one
    /// token, which is `10^(19 - decimals)` and at most 10^19. It therefore fits
    /// a u64 for every supported value of `decimals`.
    ///
    /// Only the two lending carveout pools use these two fields. Those pools are
    /// a spot market's `revenue_pool` and `protocol_fee_pool`. Both fields stay 0
    /// on every other `PoolBalance`, such as a perp market's `pnl_pool` and
    /// `fee_pool`. Both fields read 0 on markets created before the fields
    /// existed, which is the correct starting value.
    pub pending_interest_dust: u64,
}

// Layout guard. `PerpMarket` and `SpotMarket` embed `PoolBalance` at frozen
// offsets. The size must not move, and the two dust fields must start at 20 and
// 24 with no implicit `#[repr(C)]` padding. Off-chain decoders read the packed
// layout from the IDL.
const _: () = assert!(std::mem::size_of::<PoolBalance>() == 32);
const _: () = assert!(std::mem::offset_of!(PoolBalance, market_index) == 16);
const _: () = assert!(std::mem::offset_of!(PoolBalance, pending_interest_split_dust) == 20);
const _: () = assert!(std::mem::offset_of!(PoolBalance, pending_interest_dust) == 24);

impl SpotBalance for PoolBalance {
    fn market_index(&self) -> u16 {
        self.market_index
    }

    fn balance_type(&self) -> &SpotBalanceType {
        &SpotBalanceType::Deposit
    }

    fn balance(&self) -> u128 {
        self.scaled_balance
    }

    fn increase_balance(&mut self, delta: u128) -> VelocityResult {
        self.scaled_balance = self.scaled_balance.safe_add(delta)?;
        Ok(())
    }

    fn decrease_balance(&mut self, delta: u128) -> VelocityResult {
        self.scaled_balance = self.scaled_balance.safe_sub(delta)?;
        Ok(())
    }

    fn update_balance_type(&mut self, _balance_type: SpotBalanceType) -> VelocityResult {
        Err(ErrorCode::CantUpdateSpotBalanceType)
    }
}

/// Historic market data shared across all makers, updated on every fill
/// regardless of which maker filled (vAMM, DLOB resting order, JIT participant,
/// future quoter types). Holds mark/oracle TWAPs, rolling std, volume,
/// intensity, mm-oracle snapshot, `historical_oracle_data`,
/// `last_oracle_normalised_price`, `last_oracle_valid`.
///
/// Update-cadence rule: anything that needs to refresh on every market event
/// lives here. Anything AMM-private (reserves, peg, spreads — only matters
/// when the AMM specifically is the counterparty) lives on `AMM`. See
/// `docs/amm-decoupling-and-maker-interface.md`.
#[zero_copy(unsafe)]
#[derive(Debug, PartialEq, Eq)]
#[repr(C)]
pub struct MarketStats {
    /// Average estimate of (bid+ask)/2 price over funding_period.
    /// precision: PRICE_PRECISION
    pub last_mark_price_twap: u64,
    /// Average estimate of (bid+ask)/2 price over FIVE_MINUTES.
    pub last_mark_price_twap_5min: u64,
    /// The last unix_timestamp the mark twap was updated.
    pub last_mark_price_twap_ts: i64,
    /// Average estimate of bid price over funding_period.
    /// precision: PRICE_PRECISION
    pub last_bid_price_twap: u64,
    /// Average estimate of ask price over funding_period.
    /// precision: PRICE_PRECISION
    pub last_ask_price_twap: u64,
    /// Estimate of standard deviation of fill (mark) prices.
    /// precision: PRICE_PRECISION
    pub mark_std: u64,
    /// Estimate of standard deviation of the oracle price at each update.
    /// precision: PRICE_PRECISION
    pub oracle_std: u64,
    /// The pct size of the oracle confidence interval.
    /// precision: PERCENTAGE_PRECISION
    pub last_oracle_conf_pct: u64,
    /// Estimated total of volume in market.
    /// QUOTE_PRECISION
    pub volume_24h: u64,
    /// The volume intensity of long fills (across all makers).
    pub long_intensity_volume: u64,
    /// The volume intensity of short fills (across all makers).
    pub short_intensity_volume: u64,
    /// The blockchain unix_timestamp at the time of the last trade.
    pub last_trade_ts: i64,
    /// estimate of last 24h of funding rate perp market (unit is quote per base)
    /// Market-wide config / rolling stat — read by the AMM when computing
    /// `reference_price_offset` and by funding-rate updates. Migrated from
    /// `PerpMarket` so the AMM reads only from `MarketStats`.
    /// precision: QUOTE_PRECISION
    pub last_24h_avg_funding_rate: i64,
    /// the periodicity of the funding rate updates. Market-wide config used
    /// across the funding path. Migrated from `PerpMarket`.
    pub funding_period: i64,
    /// the minimum base size of an order. Market-wide config read by the AMM
    /// when computing fallback prices / spread reserves. Migrated from
    /// `PerpMarket`.
    /// precision: BASE_PRECISION
    pub min_order_size: u64,
    /// MM oracle price snapshot (set by the native handler).
    pub mm_oracle_price: i64,
    /// Slot at which the mm_oracle_* fields were last updated.
    pub mm_oracle_slot: u64,
    /// Monotonically increasing sequence id for mm_oracle updates.
    pub mm_oracle_sequence_id: u64,
    /// Canonical sanitised/clamped oracle price — the latest oracle reading
    /// after normalisation (any quoter's view, not AMM-specific).
    pub last_oracle_normalised_price: i64,
    /// Previous reference price offset, written by `_update_amm` after a
    /// successful repeg/k_update. Read by `update_amm_quote_state` to
    /// implement the legacy time-decayed reference-price-offset smoothing
    /// transition — when the freshly computed offset's sign flips relative
    /// to this cached value AND `curve_update_intensity > 100`, the
    /// transition is clamped per-slot rather than snapping. Migrated from
    /// `AMM.reference_price_offset` (which was deleted in the AMM-decoupling
    /// refactor) so the smoothing behaviour is preserved across cranks.
    /// precision: PRICE_PRECISION
    pub last_reference_price_offset: i32,
    /// Whether the oracle was valid at the most recent `_update_amm`.
    /// Read by settlement and fill paths to gate operations.
    pub last_oracle_valid: bool,
    /// Padding so last_funding_oracle_twap is 8-aligned.
    pub padding: [u8; 3],
    /// Oracle TWAP captured at last funding update, the normalizer
    /// `last_24h_avg_funding_rate` accrued against. Read by the AMM's
    /// funding bias spread and `get_last_funding_basis`. Migrated from
    /// `PerpMarket` so the AMM reads only from `MarketStats`.
    /// precision: PRICE_PRECISION
    pub last_funding_oracle_twap: i64,
    /// Historical oracle readings — TWAPs, last raw price, confidence, delay,
    /// timestamp. Market-wide data (any quoter would want it), updated by
    /// `_update_amm` / funding paths. Migrated from AMM.
    pub historical_oracle_data: HistoricalOracleData,
}

impl Default for MarketStats {
    fn default() -> Self {
        // `min_order_size: 1` preserves the old `PerpMarket::default` behaviour
        // (the field used to live on `PerpMarket`). All other fields are zero.
        Self {
            last_mark_price_twap: 0,
            last_mark_price_twap_5min: 0,
            last_mark_price_twap_ts: 0,
            last_bid_price_twap: 0,
            last_ask_price_twap: 0,
            mark_std: 0,
            oracle_std: 0,
            last_oracle_conf_pct: 0,
            volume_24h: 0,
            long_intensity_volume: 0,
            short_intensity_volume: 0,
            last_trade_ts: 0,
            last_24h_avg_funding_rate: 0,
            funding_period: 0,
            min_order_size: 1,
            mm_oracle_price: 0,
            mm_oracle_slot: 0,
            mm_oracle_sequence_id: 0,
            last_oracle_normalised_price: 0,
            last_reference_price_offset: 0,
            last_oracle_valid: false,
            padding: [0; 3],
            last_funding_oracle_twap: 0,
            historical_oracle_data: HistoricalOracleData::default(),
        }
    }
}

impl crate::state::traits::Size for MarketStats {
    const SIZE: usize = 216;
}

pub fn normalise_oracle_price(
    oracle_price_data: &OraclePriceData,
    reserve_price: u64,
) -> VelocityResult<i64> {
    let oracle_price = oracle_price_data.price;
    let reserve_price = reserve_price.cast::<i64>()?;

    // 2.5 bps of the mark price
    let reserve_price_2p5_bps = reserve_price.safe_div(4000)?;
    let conf_int = oracle_price_data.confidence.cast::<i64>()?;

    //  normalises oracle toward mark price based on the oracle’s confidence interval
    //  if mark above oracle: use oracle+conf unless it exceeds .99975 * mark price
    //  if mark below oracle: use oracle-conf unless it less than 1.00025 * mark price
    //  (this guarantees more reasonable funding rates in volatile periods)
    let normalised_price = if reserve_price > oracle_price {
        min(
            max(reserve_price.safe_sub(reserve_price_2p5_bps)?, oracle_price),
            oracle_price.safe_add(conf_int)?,
        )
    } else {
        max(
            min(reserve_price.safe_add(reserve_price_2p5_bps)?, oracle_price),
            oracle_price.safe_sub(conf_int)?,
        )
    };

    Ok(normalised_price)
}

impl MarketStats {
    /// Update the mark-price rolling-std estimate.
    pub fn update_mark_std(
        &mut self,
        now: i64,
        price: u64,
        ewma: u64,
        ewma_5min: u64,
    ) -> crate::error::VelocityResult<()> {
        self.mark_std = crate::math::stats::roll_std(
            self.mark_std,
            self.last_mark_price_twap_ts,
            now,
            price,
            ewma,
            ewma_5min,
        )?;
        Ok(())
    }

    /// Update the oracle-price rolling-std estimate.
    pub fn update_oracle_std(
        &mut self,
        now: i64,
        price: u64,
        ewma: u64,
        ewma_5min: u64,
    ) -> crate::error::VelocityResult<()> {
        self.oracle_std = crate::math::stats::roll_std(
            self.oracle_std,
            self.historical_oracle_data.last_oracle_price_twap_ts,
            now,
            price,
            ewma,
            ewma_5min,
        )?;
        Ok(())
    }

    /// Update the oracle-confidence percentage estimate using the previous
    /// value decayed as a lower bound.
    pub fn update_oracle_conf_pct(
        &mut self,
        confidence: u64,
        reserve_price: u64,
        now: i64,
    ) -> crate::error::VelocityResult<()> {
        use crate::math::{constants::BID_ASK_SPREAD_PRECISION, safe_math::SafeMath};
        let upper_bound_divisor = 21_u64;
        let lower_bound_divisor = 5_u64;
        let since_last = now
            .safe_sub(self.historical_oracle_data.last_oracle_price_twap_ts)?
            .max(0);

        let confidence_lower_bound = if since_last > 0 {
            let confidence_divisor = upper_bound_divisor
                .saturating_sub(since_last as u64)
                .max(lower_bound_divisor);
            self.last_oracle_conf_pct
                .safe_sub(self.last_oracle_conf_pct / confidence_divisor)?
        } else {
            self.last_oracle_conf_pct
        };

        self.last_oracle_conf_pct = confidence
            .safe_mul(BID_ASK_SPREAD_PRECISION)?
            .safe_div(reserve_price)?
            .max(confidence_lower_bound);
        Ok(())
    }

    /// Update volume / long-short intensity rolling sums and the last-trade
    /// timestamp on this `MarketStats`. Called from every fill path so the
    /// stats reflect total market activity across all makers.
    pub fn update_volume_24h(
        &mut self,
        quote_asset_amount: u64,
        position_direction: crate::controller::position::PositionDirection,
        now: i64,
    ) -> crate::error::VelocityResult<()> {
        use crate::math::{
            constants::{ONE_HOUR, TWENTY_FOUR_HOUR},
            safe_math::SafeMath,
            stats,
        };

        let since_last = core::cmp::max(1_i64, now.safe_sub(self.last_trade_ts)?);

        let (long_quote_amount, short_quote_amount) =
            if position_direction == crate::controller::position::PositionDirection::Long {
                (quote_asset_amount, 0_u64)
            } else {
                (0_u64, quote_asset_amount)
            };

        self.long_intensity_volume = stats::calculate_rolling_sum(
            self.long_intensity_volume,
            long_quote_amount,
            since_last,
            ONE_HOUR,
        )?;

        self.short_intensity_volume = stats::calculate_rolling_sum(
            self.short_intensity_volume,
            short_quote_amount,
            since_last,
            ONE_HOUR,
        )?;

        self.volume_24h = stats::calculate_rolling_sum(
            self.volume_24h,
            quote_asset_amount,
            since_last,
            TWENTY_FOUR_HOUR,
        )?;

        self.last_trade_ts = now;

        Ok(())
    }

    /// Discard the mark TWAPs and re-seed them from the oracle TWAPs. This runs when
    /// the mark TWAPs stay unwritten for so long that they keep no usable history.
    ///
    /// `calculate_new_twap` weights an incoming sample by the time since the last
    /// write. The opposing weight floors at 1. Past a few funding periods the next
    /// fill-path sample replaces the TWAP outright, because fills pass no
    /// `max_sample_elapsed` cap. One quote then sets that period's funding premium.
    /// The bid/ask crank's samples are capped ([`Self::max_mark_twap_sample_elapsed`]),
    /// so on that path the re-seed instead replaces a slow crawl of capped samples
    /// with one exact write of the oracle TWAP.
    ///
    /// A funding pause makes that gap longest. `handle_update_funding_rate` and
    /// `handle_update_perp_bid_ask_twap` both reject while the pause is set. A market
    /// that does not trade then has no writer at all. `on_the_hour_update` makes
    /// funding fire on the first crank after the pause lifts. The moment of that write
    /// is therefore predictable.
    ///
    /// The oracle TWAP is the correct replacement. It advances during a pause through
    /// `update_amms` and perp fills. Every caller of `update_mark_twap` also refreshes
    /// it earlier in the same instruction. The re-seed makes the premium zero for one
    /// period. The market relearns its real premium from the samples that follow.
    ///
    /// Returns whether the re-seed ran. The caller uses this to skip work that the
    /// re-seed makes moot.
    fn reseed_mark_twap_from_oracle_if_stale(
        &mut self,
        now: i64,
    ) -> crate::error::VelocityResult<bool> {
        use crate::math::{
            casting::Cast,
            constants::{MARK_TWAP_RESEED_FUNDING_PERIODS, ONE_HOUR},
            safe_math::SafeMath,
        };

        // `funding_period` is 0 on some test markets, which would make every write a
        // re-seed. The floor also keeps a market with a short funding period from
        // re-seeding on an ordinary quiet hour.
        let max_staleness = self
            .funding_period
            .safe_mul(MARK_TWAP_RESEED_FUNDING_PERIODS)?
            .max(ONE_HOUR);

        if now.safe_sub(self.last_mark_price_twap_ts)? <= max_staleness {
            return Ok(false);
        }

        let oracle_twap = self.historical_oracle_data.last_oracle_price_twap;
        self.last_bid_price_twap = oracle_twap.cast()?;
        self.last_ask_price_twap = oracle_twap.cast()?;
        self.last_mark_price_twap = oracle_twap.cast()?;
        self.last_mark_price_twap_5min = self
            .historical_oracle_data
            .last_oracle_price_twap_5min
            .cast()?;
        self.last_mark_price_twap_ts = now;

        Ok(true)
    }

    /// Ceiling on the elapsed time a single mark-TWAP sample may be weighted by.
    ///
    /// A mark-TWAP update weights the new sample by `elapsed / funding_period`,
    /// where `elapsed` is the time since the TWAP was last advanced. That weight
    /// is only honest while `elapsed` is time during which the sample could not
    /// have been chosen by whoever profits from it. The bid/ask crank breaks that
    /// property: it folds caller-supplied DLOB depth into the TWAP and can run
    /// after an arbitrarily long gap, so one caller-chosen snapshot would claim a
    /// near-full-period weight and move the TWAP (and the funding rate it feeds)
    /// in a single instruction.
    ///
    /// Capping the elapsed a single sample may claim bounds that move to
    /// `sample_deviation * cap / funding_period` no matter how large the gap, so
    /// no one crank can set the funding input. Moving it then requires holding
    /// the book across many samples, which costs real resting, fillable depth
    /// over time and stays bounded by the oracle-divergence band. The cap does
    /// not make funding manipulation-proof against an actor willing to pay that
    /// sustained cost; it removes the free, single-shot version.
    ///
    /// The value is the same staleness granularity `update_mark_twap` already
    /// uses to shrink a stale TWAP toward the oracle, so the sample-weight cap
    /// and that oracle shrink trip at one shared threshold.
    pub fn max_mark_twap_sample_elapsed(&self) -> VelocityResult<i64> {
        Ok(self.funding_period.safe_div(60)?.max(ONE_MINUTE.cast()?))
    }

    /// Update the bid/ask/mid mark-price TWAPs (funding-period and 5-minute)
    /// from a freshly-observed bid/ask pair. Pure MarketStats mutation —
    /// callers compute `bid_price` / `ask_price` from whichever liquidity
    /// source produced the fill (vAMM quote, DLOB, JIT participant).
    ///
    /// A market that does not write these TWAPs for several funding periods keeps no
    /// usable history. [`Self::reseed_mark_twap_from_oracle_if_stale`] then re-seeds
    /// them from the oracle TWAPs, and this sample lands on the next call.
    ///
    /// `max_sample_elapsed` caps the elapsed time credited to this sample (see
    /// [`MarketStats::max_mark_twap_sample_elapsed`]). Fills and the AMM re-blend
    /// pass `None` (their samples are AMM/trade-derived, not caller-curated); the
    /// bid/ask crank passes `Some(..)` so caller-supplied DLOB depth cannot claim
    /// a full-period weight after a gap. It bounds only the new sample's weight;
    /// the stale-TWAP shrink below still keys off the real last-update timestamp.
    pub fn update_mark_twap(
        &mut self,
        now: i64,
        bid_price: u64,
        ask_price: u64,
        precomputed_trade_price: Option<u64>,
        sanitize_clamp: Option<i64>,
        max_sample_elapsed: Option<i64>,
    ) -> crate::error::VelocityResult<u64> {
        let funding_period = self.funding_period;
        use {
            crate::{
                math::{
                    casting::Cast,
                    constants::FIVE_MINUTE,
                    safe_math::SafeMath,
                    stats::{calculate_new_twap, calculate_weighted_average},
                },
                validate,
                vlp::amm::math::amm::sanitize_new_price,
            },
            core::cmp::max,
        };

        // A re-seed stamps the clock to `now`. Every weighted average below then sees a
        // zero interval and returns the value the re-seed wrote. The blend reaches the
        // same answer at a cost. The sanitize step clamps this sample against the TWAP
        // that the re-seed just wrote. `update_mark_std` also measures the seeded price
        // across a one second interval that it did not observe. This must run before
        // `sample_last_ts` below, so a post-gap sample is measured against the stamp
        // the re-seed wrote, not against the gap it just consumed.
        if self.reseed_mark_twap_from_oracle_if_stale(now)? {
            return Ok(self.last_mark_price_twap);
        }

        // Timestamp the new sample is weighted against. `calculate_new_twap`
        // credits the sample `now - last_ts` of elapsed time; capping that span
        // caps the sample's weight. Uncapped callers use the real last-update
        // time; the crank passes `Some(cap)` so a post-gap sample cannot claim
        // more than `cap` of elapsed time. The stale-shrink branch below keeps
        // using the real `last_mark_price_twap_ts`, and the update still stamps
        // `last_mark_price_twap_ts = now` at the end.
        let sample_last_ts = match max_sample_elapsed {
            Some(cap) => max(self.last_mark_price_twap_ts, now.safe_sub(cap)?),
            None => self.last_mark_price_twap_ts,
        };

        let (bid_price_capped_update, ask_price_capped_update) = (
            sanitize_new_price(
                bid_price.cast()?,
                self.last_bid_price_twap.cast()?,
                sanitize_clamp,
            )?,
            sanitize_new_price(
                ask_price.cast()?,
                self.last_ask_price_twap.cast()?,
                sanitize_clamp,
            )?,
        );

        validate!(
            bid_price_capped_update <= ask_price_capped_update,
            crate::error::ErrorCode::InvalidMarkTwapUpdateDetected,
            "bid_price_capped_update not <= ask_price_capped_update,"
        )?;

        let last_valid_trade_since_oracle_twap_update = self
            .historical_oracle_data
            .last_oracle_price_twap_ts
            .safe_sub(self.last_mark_price_twap_ts)?;

        // if delayed more than ONE_MINUTE or 60th of funding period, shrink toward oracle_twap
        let (last_bid_price_twap, last_ask_price_twap) =
            if last_valid_trade_since_oracle_twap_update > self.max_mark_twap_sample_elapsed()? {
                crate::msg!(
                    "correcting mark twap update (oracle previously invalid for {:?} seconds)",
                    last_valid_trade_since_oracle_twap_update
                );

                let from_start_valid = max(
                    0,
                    funding_period.safe_sub(last_valid_trade_since_oracle_twap_update)?,
                );
                (
                    calculate_weighted_average(
                        self.historical_oracle_data
                            .last_oracle_price_twap
                            .cast::<i64>()?,
                        self.last_bid_price_twap.cast()?,
                        last_valid_trade_since_oracle_twap_update,
                        from_start_valid,
                        Some(
                            self.historical_oracle_data
                                .last_oracle_price_twap
                                .safe_sub(self.last_bid_price_twap.cast()?)?
                                .signum(),
                        ),
                    )?,
                    calculate_weighted_average(
                        self.historical_oracle_data
                            .last_oracle_price_twap
                            .cast::<i64>()?,
                        self.last_ask_price_twap.cast()?,
                        last_valid_trade_since_oracle_twap_update,
                        from_start_valid,
                        Some(
                            self.historical_oracle_data
                                .last_oracle_price_twap
                                .safe_sub(self.last_ask_price_twap.cast()?)?
                                .signum(),
                        ),
                    )?,
                )
            } else {
                (
                    self.last_bid_price_twap.cast()?,
                    self.last_ask_price_twap.cast()?,
                )
            };

        let bid_twap = calculate_new_twap(
            bid_price_capped_update,
            now,
            last_bid_price_twap,
            sample_last_ts,
            funding_period,
        )?;
        self.last_bid_price_twap = bid_twap.cast()?;

        let ask_twap = calculate_new_twap(
            ask_price_capped_update,
            now,
            last_ask_price_twap,
            sample_last_ts,
            funding_period,
        )?;
        self.last_ask_price_twap = ask_twap.cast()?;

        let mid_twap = bid_twap.safe_add(ask_twap)? / 2;

        let trade_price: u64 = match precomputed_trade_price {
            Some(trade_price) => trade_price,
            None => bid_price.safe_add(ask_price)?.safe_div(2)?,
        };
        self.update_mark_std(
            now,
            trade_price,
            self.last_mark_price_twap,
            self.last_mark_price_twap_5min,
        )?;

        self.last_mark_price_twap = mid_twap.cast()?;
        self.last_mark_price_twap_5min = calculate_new_twap(
            bid_price_capped_update
                .safe_add(ask_price_capped_update)?
                .safe_div(2)?
                .cast()?,
            now,
            self.last_mark_price_twap_5min.cast()?,
            sample_last_ts,
            FIVE_MINUTE as i64,
        )?
        .cast()?;

        self.last_mark_price_twap_ts = now;

        mid_twap.cast()
    }

    /// Update the mark-price TWAP by first estimating today's best bid/ask
    /// from the AMM's quote state (and an optional trade-price hint).
    /// `amm` is read-only; only `self` is mutated.
    /// Test-only convenience that derives the bid/ask inputs from a `&AMM`
    /// borrow and forwards to [`update_mark_twap_with_amm_bid_ask`]. Real
    /// callers (orchestrator, funding) read the bid/ask themselves and call
    /// the data-only entrypoint — in the target architecture those reads
    /// come back from the AMM module's contract methods and Velocity's general
    /// logic stops reaching for the AMM as a Rust struct directly.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn update_mark_twap_from_estimates(
        &mut self,
        amm: &AMM,
        now: i64,
        precomputed_trade_price: Option<u64>,
        direction: Option<crate::controller::position::PositionDirection>,
        sanitize_clamp: Option<i64>,
        order_tick_size: u64,
    ) -> crate::error::VelocityResult<u64> {
        let reserve_price = amm.reserve_price()?;
        let (amm_bid_price, amm_ask_price) = amm.bid_ask_price(
            reserve_price,
            amm.long_spread,
            amm.short_spread,
            amm.reference_price_offset,
        )?;
        self.update_mark_twap_with_amm_bid_ask(
            amm_bid_price,
            amm_ask_price,
            amm.base_spread,
            amm.long_spread,
            amm.short_spread,
            now,
            precomputed_trade_price,
            direction,
            sanitize_clamp,
            order_tick_size,
        )
    }

    /// Update the mark TWAP using AMM-derived bid/ask + base spread plus an
    /// optional trade-price hint. Pure-data entrypoint: no `&AMM` borrow
    /// (the AMM-derived scalars come from the caller — today via direct
    /// reads, in the target architecture via the AMM module's contract
    /// methods).
    #[allow(clippy::too_many_arguments)]
    pub fn update_mark_twap_with_amm_bid_ask(
        &mut self,
        amm_bid_price: u64,
        amm_ask_price: u64,
        amm_base_spread: u32,
        amm_long_spread: u32,
        amm_short_spread: u32,
        now: i64,
        precomputed_trade_price: Option<u64>,
        direction: Option<crate::controller::position::PositionDirection>,
        sanitize_clamp: Option<i64>,
        order_tick_size: u64,
    ) -> crate::error::VelocityResult<u64> {
        let (bid_price, ask_price) = crate::vlp::amm::math::amm::estimate_best_bid_ask_price(
            amm_bid_price,
            amm_ask_price,
            amm_base_spread,
            amm_long_spread,
            amm_short_spread,
            &self.historical_oracle_data,
            precomputed_trade_price,
            direction,
            order_tick_size,
        )?;
        // AMM/trade-derived sample, not caller-curated: no sample-weight cap.
        self.update_mark_twap(
            now,
            bid_price,
            ask_price,
            precomputed_trade_price,
            sanitize_clamp,
            None,
        )
    }

    /// Update the mark-price TWAP using the *best* of (vAMM bid/ask, DLOB
    /// bid/ask). Used by the explicit mark-twap crank to fold DLOB liquidity
    /// into the on-chain TWAP estimate.
    ///
    /// The DLOB side is caller-supplied, so this is the one path that curates
    /// its own sample. It caps the sample's elapsed weight
    /// ([`MarketStats::max_mark_twap_sample_elapsed`]) so a single crank after a
    /// gap cannot set the funding input; funding and fills stay uncapped.
    pub fn update_mark_twap_crank(
        &mut self,
        amm: &AMM,
        now: i64,
        oracle_price_data: &crate::state::oracle::OraclePriceData,
        best_dlob_bid_price: Option<u64>,
        best_dlob_ask_price: Option<u64>,
        sanitize_clamp: Option<i64>,
    ) -> crate::error::VelocityResult<()> {
        let amm_reserve_price = amm.reserve_price()?;
        let (amm_bid_price, amm_ask_price) = amm.bid_ask_price(
            amm_reserve_price,
            amm.long_spread,
            amm.short_spread,
            amm.reference_price_offset,
        )?;

        let mut best_bid_price = match best_dlob_bid_price {
            Some(best_dlob_bid_price) => best_dlob_bid_price.max(amm_bid_price),
            None => amm_bid_price,
        };
        let mut best_ask_price = match best_dlob_ask_price {
            Some(best_dlob_ask_price) => best_dlob_ask_price.min(amm_ask_price),
            None => amm_ask_price,
        };

        if best_bid_price > best_ask_price {
            let market_basis = self
                .last_mark_price_twap_5min
                .cast::<i64>()?
                .safe_sub(self.historical_oracle_data.last_oracle_price_twap_5min)?
                .clamp(
                    -oracle_price_data.price / 100,
                    oracle_price_data.price / 100,
                );
            if best_bid_price >= oracle_price_data.price.safe_add(market_basis)?.cast()? {
                best_bid_price = best_ask_price;
            } else {
                best_ask_price = best_bid_price;
            }
        }

        let max_sample_elapsed = self.max_mark_twap_sample_elapsed()?;
        self.update_mark_twap(
            now,
            best_bid_price,
            best_ask_price,
            None,
            sanitize_clamp,
            Some(max_sample_elapsed),
        )?;
        Ok(())
    }

    /// Update the oracle-price TWAP and rolling oracle-confidence stats.
    /// `amm` is read-only — only used to fall back to `amm.reserve_price()`
    /// when `precomputed_reserve_price` is `None`. Only `self` is mutated.
    pub fn update_oracle_twap(
        &mut self,
        amm: &AMM,
        now: i64,
        mm_oracle_price_data: &crate::state::oracle::MMOraclePriceData,
        precomputed_reserve_price: Option<u64>,
        sanitize_clamp: Option<i64>,
    ) -> crate::error::VelocityResult<i64> {
        let reserve_price = match precomputed_reserve_price {
            Some(reserve_price) => reserve_price,
            None => amm.reserve_price()?,
        };

        let oracle_confidence = mm_oracle_price_data.get_confidence();
        let oracle_price = normalise_oracle_price(
            &mm_oracle_price_data.get_exchange_oracle_price_data(),
            reserve_price,
        )?;

        let capped_oracle_update_price = sanitize_new_price(
            oracle_price,
            self.historical_oracle_data.last_oracle_price_twap,
            sanitize_clamp,
        )?;

        let oracle_price_twap: i64;
        if capped_oracle_update_price > 0 && oracle_price > 0 {
            oracle_price_twap = calculate_new_oracle_price_twap(
                self,
                now,
                capped_oracle_update_price,
                TwapPeriod::FundingPeriod,
            )?;

            let oracle_price_twap_5min = calculate_new_oracle_price_twap(
                self,
                now,
                capped_oracle_update_price,
                TwapPeriod::FiveMin,
            )?;

            self.last_oracle_normalised_price = capped_oracle_update_price;
            self.historical_oracle_data.last_oracle_price =
                mm_oracle_price_data.get_exchange_oracle_price_data().price;
            // (price, conf) identify the validated sample; consumers of the
            // cached `last_oracle_valid` verdict compare against these to
            // detect a same-slot oracle rewrite after the AMM update.
            self.historical_oracle_data.last_oracle_conf = mm_oracle_price_data
                .get_exchange_oracle_price_data()
                .confidence;

            let prev_oracle_twap = self.historical_oracle_data.last_oracle_price_twap;
            let prev_oracle_twap_5min = self.historical_oracle_data.last_oracle_price_twap_5min;

            self.update_oracle_conf_pct(oracle_confidence, reserve_price, now)?;

            self.historical_oracle_data.last_oracle_delay =
                mm_oracle_price_data.get_exchange_oracle_price_data().delay;

            self.update_oracle_std(
                now,
                oracle_price.cast()?,
                prev_oracle_twap.cast()?,
                prev_oracle_twap_5min.cast()?,
            )?;

            self.historical_oracle_data.last_oracle_price_twap_5min = oracle_price_twap_5min;
            self.historical_oracle_data.last_oracle_price_twap = oracle_price_twap;
            self.historical_oracle_data.last_oracle_price_twap_ts = now;
        } else {
            oracle_price_twap = self.historical_oracle_data.last_oracle_price_twap;
        }

        Ok(oracle_price_twap)
    }
}

pub use crate::vlp::{amm::state::AMM, hedge::state::HedgeConfig};
