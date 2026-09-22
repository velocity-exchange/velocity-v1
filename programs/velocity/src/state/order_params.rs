use {
    crate::{
        controller::position::PositionDirection,
        error::VelocityResult,
        math::{
            casting::Cast,
            constants::{
                ONE_HUNDRED_THOUSAND_QUOTE, PERCENTAGE_PRECISION_I64, PERCENTAGE_PRECISION_U64,
                PRICE_PRECISION_I64,
            },
            safe_math::SafeMath,
            safe_unwrap::SafeUnwrap,
            time::Millis,
        },
        state::{
            events::OrderActionExplanation,
            perp_market::{ContractTier, PerpMarket},
            user::{MarketType, OrderTriggerCondition, OrderType},
        },
    },
    anchor_lang::prelude::{
        borsh::{BorshDeserialize, BorshSerialize},
        *,
    },
    std::ops::Div,
};

#[cfg(test)]
mod tests;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Copy, Eq, PartialEq, Debug)]
pub struct OrderParams {
    pub order_type: OrderType,
    pub market_type: MarketType,
    pub direction: PositionDirection,
    pub user_order_id: u8,
    pub base_asset_amount: u64,
    pub price: u64,
    pub market_index: u16,
    pub reduce_only: bool,
    pub post_only: PostOnlyParam,
    pub bit_flags: u8,
    pub max_ts: Option<i64>,
    pub trigger_price: Option<u64>,
    pub trigger_condition: OrderTriggerCondition,
    pub oracle_price_offset: Option<i64>, // price offset from oracle for order
    pub auction_duration: Option<u8>,     // wall clock 400ms units (one slot at the 400ms baseline)
    pub auction_start_price: Option<i64>, // specified in price or oracle_price_offset
    pub auction_end_price: Option<i64>,   // specified in price or oracle_price_offset
    /// the index into the placing user's RevenueShareEscrow.approved_builders list, if this order
    /// carries a builder code. Only honored for non-swift orders; swift orders carry the builder
    /// info in the signed message envelope instead.
    pub builder_idx: Option<u8>,
    /// the builder fee on this order, in tenths of a bps, e.g. 100 = 0.01%
    pub builder_fee_tenth_bps: Option<u16>,
}

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq, Debug, Eq)]
#[borsh(use_discriminant = true)]
pub enum OrderParamsBitFlag {
    ImmediateOrCancel = 0b00000001,
}

/// The auction parameters an order carried before a sanitizing pass ran. The
/// duration floor measures against the range the order asked for, and the
/// caller reports a change by comparing against these.
struct AuctionBounds {
    duration: Option<u8>,
    start_price: Option<i64>,
    end_price: Option<i64>,
}

impl AuctionBounds {
    fn read(params: &OrderParams) -> Self {
        AuctionBounds {
            duration: params.auction_duration,
            start_price: params.auction_start_price,
            end_price: params.auction_end_price,
        }
    }
}

impl OrderParams {
    pub fn has_valid_auction_params(&self) -> VelocityResult<bool> {
        if self.auction_duration.is_some()
            && self.auction_start_price.is_some()
            && self.auction_end_price.is_some()
        {
            if self.direction == PositionDirection::Long {
                Ok(self.auction_start_price.safe_unwrap()?
                    <= self.auction_end_price.safe_unwrap()?)
            } else {
                Ok(self.auction_start_price.safe_unwrap()?
                    >= self.auction_end_price.safe_unwrap()?)
            }
        } else if self.order_type == OrderType::Limit
            && self.auction_duration.is_none()
            && self.auction_start_price.is_none()
            && self.auction_end_price.is_none()
        {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn update_perp_auction_params_limit_orders(
        &mut self,
        perp_market: &PerpMarket,
        oracle_price: i64,
        is_signed_msg: bool,
    ) -> VelocityResult<bool> {
        if self.post_only != PostOnlyParam::None {
            return Ok(false);
        }

        let requested = AuctionBounds::read(self);

        // A limit order with an oracle offset is refused at validation, so
        // this pass prices auctions off the fixed limit price only. An offset
        // order passes through unchanged and `validate_limit_order` refuses
        // it.
        if self.price == 0 {
            return Ok(false);
        }

        let is_signed_msg_non_tail_mkt = is_signed_msg
            && perp_market
                .contract_tier
                .is_as_safe_as_contract(&ContractTier::B);

        if is_signed_msg_non_tail_mkt
            && self.auction_duration.is_some()
            && self.auction_start_price.is_some()
            && self.auction_end_price.is_some()
        {
            // Signed-message limit orders also carry user-approved auction
            // parameters. On an A or B market a fully specified auction is
            // preserved, so a crossing limit can choose a short, aggressive
            // fill path. Validation and the limit price remain the hard bounds.
            return Ok(false);
        }

        let auction_start_price_offset =
            OrderParams::get_perp_baseline_start_price_offset(perp_market, self.direction)?;
        let new_auction_start_price = oracle_price.safe_add(auction_start_price_offset)?;

        if self.auction_duration.unwrap_or(0) == 0 {
            let est_vamm_price: u64 = match self.direction {
                PositionDirection::Long => {
                    let ask_premium = perp_market
                        .amm
                        .last_ask_premium(&perp_market.market_stats)?;
                    oracle_price.safe_add(ask_premium)?.cast()?
                }
                PositionDirection::Short => {
                    let bid_discount = perp_market
                        .amm
                        .last_bid_discount(&perp_market.market_stats)?;
                    oracle_price.safe_sub(bid_discount)?.cast()?
                }
            };

            let crosses_vamm = match self.direction {
                PositionDirection::Long => self.price > est_vamm_price,
                PositionDirection::Short => self.price < est_vamm_price,
            };

            if !crosses_vamm {
                return Ok(false);
            }

            let new_auction_start_price = match self.direction {
                PositionDirection::Long => new_auction_start_price.min(est_vamm_price as i64),
                PositionDirection::Short => new_auction_start_price.max(est_vamm_price as i64),
            };

            msg!(
                "Updating auction start price to {}",
                new_auction_start_price
            );

            self.auction_start_price = Some(new_auction_start_price);
            msg!("Updating auction end price to {}", self.price);
            self.auction_end_price = Some(self.price as i64);
        } else {
            match self.auction_start_price {
                Some(auction_start_price) => {
                    let threshold_long = if is_signed_msg {
                        auction_start_price.safe_sub(auction_start_price.abs().safe_div(1000)?)?
                    } else {
                        auction_start_price
                    };
                    let threshold_short = if is_signed_msg {
                        auction_start_price.safe_add(auction_start_price.abs().safe_div(1000)?)?
                    } else {
                        auction_start_price
                    };
                    let improves_long = self.direction == PositionDirection::Long
                        && new_auction_start_price < threshold_long;

                    let improves_short = self.direction == PositionDirection::Short
                        && new_auction_start_price > threshold_short;

                    if improves_long || improves_short {
                        msg!(
                            "Updating limit auction start price to {}",
                            new_auction_start_price
                        );
                        self.auction_start_price = Some(new_auction_start_price);
                    }
                }
                None => {
                    msg!(
                        "Updating limit auction start price to {}",
                        new_auction_start_price
                    );

                    self.auction_start_price = Some(new_auction_start_price);
                }
            }

            if self.auction_end_price.is_none() {
                msg!("Updating limit auction end price to {}", self.price);
                self.auction_end_price = Some(self.price as i64);
            }
        }

        let worst_price = self.price as i64;

        if self.direction == PositionDirection::Long {
            if let Some(auction_start_price) = self.auction_start_price {
                self.auction_start_price = Some(auction_start_price.min(worst_price));
            }
            if let Some(auction_end_price) = self.auction_end_price {
                self.auction_end_price = Some(auction_end_price.min(worst_price));
            }
        } else {
            if let Some(auction_start_price) = self.auction_start_price {
                self.auction_start_price = Some(auction_start_price.max(worst_price));
            }
            if let Some(auction_end_price) = self.auction_end_price {
                self.auction_end_price = Some(auction_end_price.max(worst_price));
            }
        }

        self.apply_auction_duration(perp_market, oracle_price, is_signed_msg, requested)
    }

    /// Widen the auction duration to the floor the sanitized band asks for, then
    /// report whether this pass moved any of the three auction parameters.
    ///
    /// A signed-message duration is left alone while it stays within about 4
    /// seconds of the derived one, so the client keeps its chosen fill path.
    fn apply_auction_duration(
        &mut self,
        perp_market: &PerpMarket,
        oracle_price: i64,
        is_signed_msg: bool,
        requested: AuctionBounds,
    ) -> VelocityResult<bool> {
        let auction_duration_before = self.auction_duration;
        let new_auction_duration = get_auction_duration(
            self.get_duration_floor_price_diff(requested.start_price, requested.end_price)?,
            oracle_price.unsigned_abs(),
            perp_market.contract_tier,
        )?;

        let duration_tolerance = Millis::from_secs(4)
            .div_periods(Millis::UNIT)
            .min(u8::MAX as u64) as u8;

        if auction_duration_before
            .unwrap_or(0)
            .abs_diff(new_auction_duration)
            > duration_tolerance
            || !is_signed_msg
        {
            self.auction_duration = Some(
                auction_duration_before
                    .unwrap_or(0)
                    .max(new_auction_duration),
            );

            msg!(
                "Updating auction duration to {}",
                self.auction_duration.safe_unwrap()?
            );
        }

        Ok(requested.duration != self.auction_duration
            || requested.start_price != self.auction_start_price
            || requested.end_price != self.auction_end_price)
    }

    pub fn get_auction_start_price_offset(self, oracle_price: i64) -> VelocityResult<i64> {
        let start_offset = if self.order_type == OrderType::Oracle {
            self.auction_start_price.unwrap_or(0)
        } else if let Some(auction_start_price) = self.auction_start_price {
            auction_start_price.safe_sub(oracle_price)?
        } else {
            return Ok(0);
        };

        Ok(start_offset)
    }

    pub fn get_auction_end_price_offset(self, oracle_price: i64) -> VelocityResult<i64> {
        let end_offset = if self.order_type == OrderType::Oracle {
            self.auction_end_price.unwrap_or(0)
        } else if let Some(auction_end_price) = self.auction_end_price {
            auction_end_price.safe_sub(oracle_price)?
        } else {
            return Ok(0);
        };

        Ok(end_offset)
    }

    /// Price diff for the duration floor: the narrower of the requested and
    /// sanitized auction ranges (sanitized only if the order didn't supply
    /// auction prices), so sanitizing the start price toward baseline doesn't
    /// inflate the floor
    fn get_duration_floor_price_diff(
        &self,
        requested_start: Option<i64>,
        requested_end: Option<i64>,
    ) -> VelocityResult<u64> {
        let sanitized_price_diff = self
            .auction_end_price
            .safe_unwrap()?
            .safe_sub(self.auction_start_price.safe_unwrap()?)?
            .unsigned_abs();

        match (requested_start, requested_end) {
            (Some(requested_start), Some(requested_end)) => Ok(requested_end
                .safe_sub(requested_start)?
                .unsigned_abs()
                .min(sanitized_price_diff)),
            _ => Ok(sanitized_price_diff),
        }
    }

    pub fn update_perp_auction_params_market_and_oracle_orders(
        &mut self,
        perp_market: &PerpMarket,
        oracle_price: i64,
        is_market_order: bool,
        is_signed_msg: bool,
    ) -> VelocityResult<bool> {
        let requested = AuctionBounds::read(self);

        if self.auction_duration.is_none()
            || self.auction_start_price.is_none()
            || self.auction_end_price.is_none()
        {
            let (auction_start_price, auction_end_price, auction_duration) = if is_market_order {
                OrderParams::derive_market_order_auction_params(
                    perp_market,
                    self.direction,
                    oracle_price,
                    self.price,
                    PERCENTAGE_PRECISION_I64 / 400, // 25 bps
                )?
            } else {
                OrderParams::derive_oracle_order_auction_params(
                    perp_market,
                    self.direction,
                    oracle_price,
                    self.oracle_price_offset,
                    PERCENTAGE_PRECISION_I64 / 400, // 25 bps
                )?
            };

            self.auction_start_price = Some(auction_start_price);
            self.auction_end_price = Some(auction_end_price);
            self.auction_duration = Some(auction_duration);

            msg!(
                "Updating auction start price to {}",
                self.auction_start_price.safe_unwrap()?
            );

            msg!(
                "Updating auction end price to {}",
                self.auction_end_price.safe_unwrap()?
            );

            msg!(
                "Updating auction duration to {}",
                self.auction_duration.safe_unwrap()?
            );

            return Ok(true);
        }

        let is_signed_msg_non_tail_mkt = is_signed_msg
            && perp_market
                .contract_tier
                .is_as_safe_as_contract(&ContractTier::B);

        if is_signed_msg_non_tail_mkt {
            // Signed-message orders carry user-approved auction parameters. On
            // A/B markets, leave fully specified auctions to the client so it can
            // choose fast/aggressive fills; validation and user price limits are
            // still enforced when the order is built.
            return Ok(false);
        }

        // only update auction start price if the contract tier isn't Isolated
        if perp_market.can_sanitize_market_order_auctions() {
            let (new_start_price_offset, new_end_price_offset) =
                OrderParams::get_perp_baseline_start_end_price_offset(
                    perp_market,
                    self.direction,
                    2,
                )?;
            let current_start_price_offset = self.get_auction_start_price_offset(oracle_price)?;
            let current_end_price_offset = self.get_auction_end_price_offset(oracle_price)?;

            let is_tail_mkt = !perp_market
                .contract_tier
                .is_as_safe_as_contract(&ContractTier::B);

            match self.direction {
                PositionDirection::Long => {
                    let long_start_threshold = if is_signed_msg || is_tail_mkt {
                        new_start_price_offset.safe_add(oracle_price.abs().safe_div(1000)?)?
                    } else {
                        new_start_price_offset
                    };
                    let long_end_threshold = if is_signed_msg || is_tail_mkt {
                        new_end_price_offset.safe_add(oracle_price.abs().safe_div(1000)?)?
                    } else {
                        new_end_price_offset
                    };
                    if current_start_price_offset > long_start_threshold {
                        self.auction_start_price = if !is_market_order {
                            Some(new_start_price_offset)
                        } else {
                            Some(new_start_price_offset.safe_add(oracle_price)?)
                        };
                        msg!(
                            "Updating auction start price to {}",
                            self.auction_start_price.safe_unwrap()?
                        );
                    }

                    if current_end_price_offset > long_end_threshold {
                        self.auction_end_price = if !is_market_order {
                            Some(new_end_price_offset)
                        } else {
                            Some(new_end_price_offset.safe_add(oracle_price)?)
                        };
                        msg!(
                            "Updating auction end price to {}",
                            self.auction_end_price.safe_unwrap()?
                        );
                    }
                }
                PositionDirection::Short => {
                    let short_start_threshold = if is_signed_msg || is_tail_mkt {
                        new_start_price_offset.safe_sub(oracle_price.abs().safe_div(1000)?)?
                    } else {
                        new_start_price_offset
                    };
                    let short_end_threshold = if is_signed_msg || is_tail_mkt {
                        new_end_price_offset.safe_sub(oracle_price.abs().safe_div(1000)?)?
                    } else {
                        new_end_price_offset
                    };
                    if current_start_price_offset < short_start_threshold {
                        self.auction_start_price = if !is_market_order {
                            Some(new_start_price_offset)
                        } else {
                            Some(new_start_price_offset.safe_add(oracle_price)?)
                        };
                        msg!(
                            "Updating auction start price to {}",
                            self.auction_start_price.safe_unwrap()?
                        );
                    }

                    if current_end_price_offset < short_end_threshold {
                        self.auction_end_price = if !is_market_order {
                            Some(new_end_price_offset)
                        } else {
                            Some(new_end_price_offset.safe_add(oracle_price)?)
                        };
                        msg!(
                            "Updating auction end price to {}",
                            self.auction_end_price.safe_unwrap()?
                        );
                    }
                }
            }
        }

        self.apply_auction_duration(perp_market, oracle_price, is_signed_msg, requested)
    }

    pub fn derive_market_order_auction_params(
        perp_market: &PerpMarket,
        direction: PositionDirection,
        oracle_price: i64,
        limit_price: u64,
        start_buffer: i64,
    ) -> VelocityResult<(i64, i64, u8)> {
        // A limit price bounds the auction, so the end offset may reach twice the baseline buffer.
        let end_buffer_scalar = if limit_price != 0 { 2 } else { 1 };

        let (auction_start_price_offset, auction_end_price_offset) =
            OrderParams::get_perp_baseline_start_end_price_offset(
                perp_market,
                direction,
                end_buffer_scalar,
            )?;

        let mut auction_start_price = oracle_price.safe_add(auction_start_price_offset)?;
        let mut auction_end_price = oracle_price.safe_add(auction_end_price_offset)?;

        if start_buffer != 0 {
            let start_buffer_price = oracle_price
                .safe_mul(start_buffer)?
                .safe_div(PERCENTAGE_PRECISION_I64)?;

            if direction == PositionDirection::Long {
                auction_start_price = auction_start_price.safe_sub(start_buffer_price)?;
            } else {
                auction_start_price = auction_start_price.safe_add(start_buffer_price)?;
            }

            // also apply to end_price if more aggressive
            if start_buffer < 0 {
                if direction == PositionDirection::Long {
                    auction_end_price = auction_end_price.safe_sub(start_buffer_price)?;
                } else {
                    auction_end_price = auction_end_price.safe_add(start_buffer_price)?;
                }
            }
        }

        if limit_price != 0 {
            let limit_price = limit_price as i64;
            if direction == PositionDirection::Long {
                auction_start_price = auction_start_price.min(limit_price);
                auction_end_price = auction_end_price.min(limit_price);
            } else {
                auction_start_price = auction_start_price.max(limit_price);
                auction_end_price = auction_end_price.max(limit_price);
            }
        }

        let auction_duration = get_auction_duration(
            auction_end_price
                .safe_sub(auction_start_price)?
                .unsigned_abs(),
            oracle_price.unsigned_abs(),
            perp_market.contract_tier,
        )?;

        Ok((auction_start_price, auction_end_price, auction_duration))
    }

    pub fn derive_oracle_order_auction_params(
        perp_market: &PerpMarket,
        direction: PositionDirection,
        oracle_price: i64,
        oracle_price_offset: Option<i64>,
        start_buffer: i64,
    ) -> VelocityResult<(i64, i64, u8)> {
        let (mut auction_start_price, mut auction_end_price) = if let Some(oracle_price_offset) =
            oracle_price_offset
        {
            let mut auction_start_price_offset =
                OrderParams::get_perp_baseline_start_price_offset(perp_market, direction)?;

            if direction == PositionDirection::Long {
                auction_start_price_offset = auction_start_price_offset.min(oracle_price_offset)
            } else {
                auction_start_price_offset = auction_start_price_offset.max(oracle_price_offset)
            };

            (auction_start_price_offset, oracle_price_offset)
        } else {
            let (auction_start_price_offset, auction_end_price_offset) =
                OrderParams::get_perp_baseline_start_end_price_offset(perp_market, direction, 1)?;

            (auction_start_price_offset, auction_end_price_offset)
        };

        if start_buffer != 0 {
            let start_buffer_price = oracle_price
                .safe_mul(start_buffer)?
                .safe_div(PERCENTAGE_PRECISION_I64)?;

            if direction == PositionDirection::Long {
                auction_start_price = auction_start_price.safe_sub(start_buffer_price)?;
            } else {
                auction_start_price = auction_start_price.safe_add(start_buffer_price)?;
            }

            // also apply to end_price if more aggressive
            if start_buffer < 0 {
                if direction == PositionDirection::Long {
                    auction_end_price = auction_end_price.safe_sub(start_buffer_price)?;
                } else {
                    auction_end_price = auction_end_price.safe_add(start_buffer_price)?;
                }
            }
        }

        let auction_duration = get_auction_duration(
            auction_end_price
                .safe_sub(auction_start_price)?
                .unsigned_abs(),
            oracle_price.unsigned_abs(),
            perp_market.contract_tier,
        )?;

        Ok((auction_start_price, auction_end_price, auction_duration))
    }

    pub fn update_perp_auction_params(
        &mut self,
        perp_market: &PerpMarket,
        oracle_price: i64,
        is_signed_msg: bool,
    ) -> VelocityResult<bool> {
        // A test build skips auction sanitizing. Integration fixtures build
        // auctions this pass would otherwise rewrite, which changes what a
        // test asserts on. Read a test result on auction pricing with that
        // in mind. This binds the unused parameters to silence a warning.
        #[cfg(feature = "anchor-test")]
        {
            let _ = (perp_market, oracle_price, is_signed_msg);
            Ok(false)
        }

        #[cfg(not(feature = "anchor-test"))]
        let sanitized: bool = match self.order_type {
            OrderType::Limit => self.update_perp_auction_params_limit_orders(
                perp_market,
                oracle_price,
                is_signed_msg,
            )?,
            OrderType::Market | OrderType::Oracle => self
                .update_perp_auction_params_market_and_oracle_orders(
                    perp_market,
                    oracle_price,
                    self.order_type == OrderType::Market,
                    is_signed_msg,
                )?,
            _ => false,
        };

        #[cfg(not(feature = "anchor-test"))]
        Ok(sanitized)
    }

    /// Widest distance from the oracle TWAP a baseline auction start offset
    /// may sit, bounded by the tier's own widest auction via
    /// `get_auction_end_min_max_divisors` (OtterSec #146). The bound is
    /// symmetric, since the offset is signed and a manipulated input can push it either way. A zero TWAP degenerates to a zero bound, like the end buffer clamp.
    fn get_perp_baseline_max_price_offset(perp_market: &PerpMarket) -> VelocityResult<i64> {
        let oracle_twap = perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap
            .unsigned_abs();
        let (_, max_divisor) = perp_market.get_auction_end_min_max_divisors()?;

        oracle_twap.safe_div(max_divisor)?.cast::<i64>()
    }

    /// Baseline auction start offset from the oracle price for a perp auction,
    /// per side.
    ///
    /// The result is clamped symmetrically to
    /// `get_perp_baseline_max_price_offset`. Both inputs are TWAPs a caller can
    /// influence. `update_perp_bid_ask_twap` samples the book from
    /// caller-supplied `User` accounts, and past 50bps of fast and slow
    /// divergence this function uses `last_mark_price_twap_5min` alone. These
    /// offsets set the auction band for a third party's forced close, so a
    /// moved TWAP prices a stranger's exit. `BID_ASK_TWAP_MIN_QUOTE_REST`
    /// raises the cost of moving those TWAPs. This clamp bounds the damage
    /// when one still moves.
    ///
    /// The sibling `get_perp_baseline_start_end_price_offset` clamps its end
    /// buffer to the same tier band. It derives the end offset with a `min` or
    /// a `max` against the start offset, so clamping the start cannot invert
    /// start and end.
    pub fn get_perp_baseline_start_price_offset(
        perp_market: &PerpMarket,
        direction: PositionDirection,
    ) -> VelocityResult<i64> {
        let max_price_offset = OrderParams::get_perp_baseline_max_price_offset(perp_market)?;

        if perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap_ts
            .safe_sub(perp_market.market_stats.last_mark_price_twap_ts)?
            .abs()
            >= 60
            || perp_market.market_stats.volume_24h <= ONE_HUNDRED_THOUSAND_QUOTE
        {
            // if uncertain with timestamp mismatch, enforce within N bps
            let price_divisor = if perp_market
                .contract_tier
                .is_as_safe_as_contract(&ContractTier::B)
            {
                500
            } else {
                100
            };

            let uncertain_start_price_offset = match direction {
                PositionDirection::Long => {
                    perp_market.market_stats.last_bid_price_twap.cast::<i64>()? / price_divisor
                }
                PositionDirection::Short => {
                    -(perp_market.market_stats.last_ask_price_twap.cast::<i64>()? / price_divisor)
                }
            };

            // This branch divides a mark TWAP rather than the oracle TWAP, so
            // a mark far from the oracle can leave the tier band. Clamp it
            // too.
            return Ok(uncertain_start_price_offset.clamp(-max_price_offset, max_price_offset));
        }

        // price offsets baselines for perp market auctions
        let mark_twap_slow = match direction {
            PositionDirection::Long => perp_market.market_stats.last_bid_price_twap,
            PositionDirection::Short => perp_market.market_stats.last_ask_price_twap,
        }
        .cast::<i64>()?;

        let baseline_start_price_offset_fast = perp_market
            .market_stats
            .last_mark_price_twap_5min
            .cast::<i64>()?
            .safe_sub(
                perp_market
                    .market_stats
                    .historical_oracle_data
                    .last_oracle_price_twap_5min,
            )?;

        let baseline_start_price_offset_slow = mark_twap_slow.safe_sub(
            perp_market
                .market_stats
                .historical_oracle_data
                .last_oracle_price_twap,
        )?;

        let baseline_start_price_offset = if baseline_start_price_offset_slow
            .abs_diff(baseline_start_price_offset_fast)
            <= perp_market.market_stats.last_mark_price_twap_5min / 200
        {
            let frac_of_long_spread_in_price: i64 = perp_market
                .amm
                .long_spread
                .cast::<i64>()?
                .safe_mul(mark_twap_slow)?
                .safe_div(PRICE_PRECISION_I64 * 10)?;

            let frac_of_short_spread_in_price: i64 = perp_market
                .amm
                .short_spread
                .cast::<i64>()?
                .safe_mul(mark_twap_slow)?
                .safe_div(PRICE_PRECISION_I64 * 10)?;

            match direction {
                PositionDirection::Long => baseline_start_price_offset_slow
                    .safe_add(frac_of_long_spread_in_price)?
                    .min(baseline_start_price_offset_fast.safe_sub(frac_of_short_spread_in_price)?),
                PositionDirection::Short => baseline_start_price_offset_slow
                    .safe_sub(frac_of_short_spread_in_price)?
                    .max(baseline_start_price_offset_fast.safe_add(frac_of_long_spread_in_price)?),
            }
        } else {
            // more than 50bps different of fast/slow twap, use fast only
            baseline_start_price_offset_fast
        };

        Ok(baseline_start_price_offset.clamp(-max_price_offset, max_price_offset))
    }

    pub fn get_perp_baseline_start_end_price_offset(
        perp_market: &PerpMarket,
        direction: PositionDirection,
        end_buffer_scalar: u64,
    ) -> VelocityResult<(i64, i64)> {
        let oracle_twap = perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap
            .unsigned_abs();
        let baseline_start_price_offset =
            OrderParams::get_perp_baseline_start_price_offset(perp_market, direction)?;
        let (min_divisor, max_divisor) = perp_market.get_auction_end_min_max_divisors()?;

        let amm_spread_side_pct = if direction == PositionDirection::Short {
            perp_market.amm.short_spread
        } else {
            perp_market.amm.long_spread
        };

        let mut baseline_end_price_buffer = perp_market
            .market_stats
            .mark_std
            .max(perp_market.market_stats.oracle_std)
            .max(
                amm_spread_side_pct
                    .cast::<u64>()?
                    .safe_mul(oracle_twap)?
                    .safe_div(PERCENTAGE_PRECISION_U64)?,
            );
        if end_buffer_scalar >= 1 {
            baseline_end_price_buffer = baseline_end_price_buffer.safe_mul(end_buffer_scalar)?
        }
        baseline_end_price_buffer =
            baseline_end_price_buffer.clamp(oracle_twap / min_divisor, oracle_twap / max_divisor);

        let baseline_end_price_offset = if direction == PositionDirection::Short {
            let auction_end_price = perp_market
                .market_stats
                .last_bid_price_twap
                .safe_sub(baseline_end_price_buffer)?
                .cast::<i64>()?
                .safe_sub(
                    perp_market
                        .market_stats
                        .historical_oracle_data
                        .last_oracle_price_twap,
                )?;
            auction_end_price.min(baseline_start_price_offset)
        } else {
            let auction_end_price = perp_market
                .market_stats
                .last_ask_price_twap
                .safe_add(baseline_end_price_buffer)?
                .cast::<i64>()?
                .safe_sub(
                    perp_market
                        .market_stats
                        .historical_oracle_data
                        .last_oracle_price_twap,
                )?;

            auction_end_price.max(baseline_start_price_offset)
        };

        Ok((baseline_start_price_offset, baseline_end_price_offset))
    }

    pub fn get_close_perp_params(
        market: &PerpMarket,
        direction_to_close: PositionDirection,
        base_asset_amount: u64,
    ) -> VelocityResult<OrderParams> {
        let (auction_start_price, auction_end_price) =
            OrderParams::get_perp_baseline_start_end_price_offset(market, direction_to_close, 1)?;
        // 32 seconds, in wall clock 400ms units.
        let auction_duration = Millis::from_secs(32)
            .div_periods(Millis::UNIT)
            .min(u8::MAX as u64)
            .cast::<u8>()?;

        let params = OrderParams {
            market_type: MarketType::Perp,
            direction: direction_to_close,
            order_type: OrderType::Oracle,
            market_index: market.market_index,
            base_asset_amount,
            reduce_only: true,
            auction_start_price: Some(auction_start_price),
            auction_end_price: Some(auction_end_price),
            auction_duration: Some(auction_duration),
            oracle_price_offset: Some(auction_end_price.cast()?),
            ..OrderParams::default()
        };

        Ok(params)
    }

    pub fn is_immediate_or_cancel(&self) -> bool {
        self.bit_flags & OrderParamsBitFlag::ImmediateOrCancel as u8 != 0
    }

    pub fn is_max_leverage_order(&self) -> bool {
        self.base_asset_amount == u64::MAX
    }

    pub fn is_trigger_order(&self) -> bool {
        self.order_type == OrderType::TriggerMarket || self.order_type == OrderType::TriggerLimit
    }
}

/// Network tag on a signed message, checked against the build's own
/// cluster. Without it a devnet-signed message is byte-identical to a
/// mainnet one and could replay across clusters. The tag is required: an
/// absent tag replays exactly like a wrong one, so it is refused. Value is `b'm'` or `b'd'`.
pub const SIGNED_MSG_NETWORK_MAINNET: u8 = b'm';
pub const SIGNED_MSG_NETWORK_DEVNET: u8 = b'd';

/// The tag this build accepts.
pub const fn expected_signed_msg_network() -> u8 {
    #[cfg(feature = "mainnet-beta")]
    {
        SIGNED_MSG_NETWORK_MAINNET
    }
    #[cfg(not(feature = "mainnet-beta"))]
    {
        SIGNED_MSG_NETWORK_DEVNET
    }
}

/// Cap on a signed route. See [`SignedMsgOrderParamsMessage::route`]. A message naming more
/// custom quoters than this is refused, not truncated, and it bounds the message length for
/// off-chain buffers. It matches [`crate::state::prop_amm::MAX_ROUTE_QUOTERS`], since a taker
/// may name every entry one transaction can carry. Raising the cap later keeps every earlier message valid, since the field is a borsh `Vec`.
pub const MAX_SIGNED_MSG_ROUTE_LEN: usize = crate::state::prop_amm::MAX_ROUTE_QUOTERS;

/// Digest of a signed route: the `QuoterV0` entries a taker chose, reduced
/// to the bytes [`crate::state::signed_msg_user::SignedMsgOrderId`] can
/// hold. An order stores it to pin which entries a fill may claim its
/// signer chose. The route is canonicalised first, by sorting and removing duplicates, so the same choice always digests the same way, and an empty route digests to zero so one equality check covers both an unsigned and a signed route.
pub type RouteDigest = [u8; ROUTE_DIGEST_LEN];

/// Width of a [`RouteDigest`]. See [`route_digest`] for why it is this wide.
pub const ROUTE_DIGEST_LEN: usize = 8;

/// The digest of no route. A directly-placed order holds this, so one equality
/// check covers both an unsigned route and the route that was signed.
pub const NO_ROUTE_DIGEST: RouteDigest = [0; ROUTE_DIGEST_LEN];

/// Reduce `route` to a [`RouteDigest`]. Eight bytes, because a filler can
/// only claim entries the transaction carries, at most `MAX_ROUTE_QUOTERS`
/// of them, so its candidate set is every subset of that size over the
/// market's registered entries. That set is public and worth precomputing,
/// and the width targets a larger future entry count. A collision routes
/// to an entry the taker did not pick. The taker's own limit price bounds
/// that, so it is not a theft, but it still defeats the digest's purpose.
/// The width matches [`crate::state::signed_msg_user::SignedMsgOrderId`]'s stride. `Order` allows only five bytes, since a sixth byte would shift that array's stride in `User`.
pub fn route_digest(route: &[Pubkey]) -> RouteDigest {
    if route.is_empty() {
        return [0; ROUTE_DIGEST_LEN];
    }

    let mut keys: Vec<[u8; 32]> = route.iter().map(|key| key.to_bytes()).collect();
    keys.sort_unstable();
    keys.dedup();
    let flat: Vec<u8> = keys.concat();
    let hash = solana_program::hash::hash(&flat);
    let mut out = [0u8; ROUTE_DIGEST_LEN];
    out.copy_from_slice(&hash.to_bytes()[..ROUTE_DIGEST_LEN]);
    // A real route must stay distinguishable from an absent one, so it never
    // takes the no-route value.
    if out == [0; ROUTE_DIGEST_LEN] {
        out[0] = 1;
    }

    out
}

/// Trailing fields are appended, never inserted. The verifier zero-pads a
/// short payload before decoding, so an older producer's message reads as
/// `None` for everything it did not send. See
/// `validation::sig_verification`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Eq, PartialEq, Debug)]
pub struct SignedMsgOrderParamsMessage {
    pub signed_msg_order_params: OrderParams,
    pub sub_account_id: u16,
    pub slot: u64,
    pub uuid: [u8; 8],
    pub take_profit_order_params: Option<SignedMsgTriggerOrderParams>,
    pub stop_loss_order_params: Option<SignedMsgTriggerOrderParams>,
    pub max_margin_ratio: Option<u16>,
    pub builder_idx: Option<u8>,
    pub builder_fee_tenth_bps: Option<u16>,
    pub isolated_position_deposit: Option<u64>,
    /// [`SIGNED_MSG_NETWORK_MAINNET`] / [`SIGNED_MSG_NETWORK_DEVNET`].
    pub network: Option<u8>,
    /// The route the taker signed for: the `QuoterV0` entries of the custom
    /// quoters (PropAMMs) the taker wants used. The CLOB and vAMM are the
    /// mandatory baseline of every router fill, so they are implicit and
    /// never named here. The field is advisory today. Swift forwards it to keepers, which is what makes a routed order reach the quoters the taker chose.
    pub route: Option<Vec<Pubkey>>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Eq, PartialEq, Debug)]
pub struct SignedMsgOrderParamsDelegateMessage {
    pub signed_msg_order_params: OrderParams,
    pub taker_pubkey: Pubkey,
    pub slot: u64,
    pub uuid: [u8; 8],
    pub take_profit_order_params: Option<SignedMsgTriggerOrderParams>,
    pub stop_loss_order_params: Option<SignedMsgTriggerOrderParams>,
    pub max_margin_ratio: Option<u16>,
    pub builder_idx: Option<u8>,
    pub builder_fee_tenth_bps: Option<u16>,
    pub isolated_position_deposit: Option<u64>,
    /// See [`SignedMsgOrderParamsMessage::network`].
    pub network: Option<u8>,
    /// See [`SignedMsgOrderParamsMessage::route`].
    pub route: Option<Vec<Pubkey>>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default, Eq, PartialEq, Debug)]
pub struct SignedMsgTriggerOrderParams {
    pub trigger_price: u64,
    pub base_asset_amount: u64,
}

fn get_auction_duration(
    price_diff: u64,
    price: u64,
    contract_tier: ContractTier,
) -> VelocityResult<u8> {
    let percent_diff = price_diff.safe_mul(PERCENTAGE_PRECISION_U64)?.div(price);

    // Duration granted per 1% of price difference. The step is the historical
    // 400ms calibration, so 100 steps per 1% is 40s and 60 steps per 1% is
    // 24s.
    let steps_per_pct = if contract_tier.is_as_safe_as_contract(&ContractTier::B) {
        100
    } else {
        60
    };

    // `Order.auction_duration` stores wall clock 400ms units rather than
    // live slots, so the value does not depend on the slot duration. The
    // clamp below stops at 180 units (72s); the type ceiling of 255 units
    // is 102s. Auction progress converts elapsed slots to wall clock through the `SlotClock` at fill time.
    Ok(percent_diff
        .safe_mul(steps_per_pct)?
        .safe_div_ceil(PERCENTAGE_PRECISION_U64 / 100)?
        .clamp(1, 180) as u8) // ~72s max
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq, Default)]
pub enum PostOnlyParam {
    #[default]
    None,
    MustPostOnly, // Tx fails if order can't be post only
    TryPostOnly,  // Tx succeeds and order not placed if can't be post only
    Slide,        // Modify price to be post only if can't be post only
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct ModifyOrderParams {
    pub direction: Option<PositionDirection>,
    pub base_asset_amount: Option<u64>,
    pub price: Option<u64>,
    pub reduce_only: Option<bool>,
    pub post_only: Option<PostOnlyParam>,
    pub bit_flags: Option<u8>,
    pub max_ts: Option<i64>,
    pub trigger_price: Option<u64>,
    pub trigger_condition: Option<OrderTriggerCondition>,
    pub oracle_price_offset: Option<i64>,
    pub auction_duration: Option<u8>,
    pub auction_start_price: Option<i64>,
    pub auction_end_price: Option<i64>,
    pub policy: Option<u8>,
}

impl ModifyOrderParams {
    pub fn must_modify(&self) -> bool {
        self.policy.unwrap_or(0) & ModifyOrderPolicy::MustModify as u8 != 0
    }

    pub fn exclude_previous_fill(&self) -> bool {
        self.policy.unwrap_or(0) & ModifyOrderPolicy::ExcludePreviousFill as u8 != 0
    }
}

pub enum ModifyOrderPolicy {
    MustModify = 1,
    ExcludePreviousFill = 2,
}

#[derive(Clone)]
pub struct PlaceOrderOptions {
    pub signed_msg_taker_order_slot: Option<u64>,
    pub try_expire_orders: bool,
    pub enforce_margin_check: bool,
    pub risk_increasing: bool,
    pub explanation: OrderActionExplanation,
    pub existing_position_direction_override: Option<PositionDirection>,
    /// Emit the `Place` `OrderActionRecord` and `OrderRecord` for the built
    /// order. A maker resting straight on the CLOB clears this flag: its
    /// CLOB placement record is the one statement about the order, and an
    /// detached place record would duplicate it.
    pub emit_place_record: bool,
}

impl Default for PlaceOrderOptions {
    fn default() -> Self {
        Self {
            signed_msg_taker_order_slot: None,
            try_expire_orders: true,
            enforce_margin_check: true,
            risk_increasing: false,
            explanation: OrderActionExplanation::None,
            existing_position_direction_override: None,
            emit_place_record: true,
        }
    }
}

impl PlaceOrderOptions {
    pub fn update_risk_increasing(&mut self, risk_increasing: bool) {
        self.risk_increasing = self.risk_increasing || risk_increasing;
    }

    pub fn explanation(mut self, explanation: OrderActionExplanation) -> Self {
        self.explanation = explanation;
        self
    }

    pub fn is_liquidation(&self) -> bool {
        self.explanation == OrderActionExplanation::Liquidation
    }

    pub fn set_order_slot(&mut self, slot: u64) {
        self.signed_msg_taker_order_slot = Some(slot);
    }

    pub fn get_order_slot(&self, order_slot: u64) -> u64 {
        let mut min_order_slot = order_slot;
        if let Some(signed_msg_taker_order_slot) = self.signed_msg_taker_order_slot {
            min_order_slot = order_slot.min(signed_msg_taker_order_slot);
        }
        min_order_slot
    }

    pub fn is_signed_msg_order(&self) -> bool {
        self.signed_msg_taker_order_slot.is_some()
    }
}

pub enum PlaceAndTakeOrderSuccessCondition {
    PartialFill = 1,
    FullFill = 2,
}

pub fn parse_optional_params(optional_params: Option<u32>) -> (u8, u8) {
    match optional_params {
        Some(optional_params) => (
            (optional_params & 255) as u8,
            ((optional_params >> 8) & 255) as u8,
        ),
        None => (0, 100),
    }
}
