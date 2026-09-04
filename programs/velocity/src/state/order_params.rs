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

        let auction_duration = self.auction_duration;
        let auction_start_price = self.auction_start_price;
        let auction_end_price = self.auction_end_price;

        // A limit order with an oracle offset is refused at validation, so
        // this pass prices auctions off the fixed limit price only. An offset
        // order falls through untouched and dies in `validate_limit_order`.
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
            // parameters. On A/B markets, preserve fully specified auctions so a
            // crossing limit can choose a short/aggressive fill path; validation
            // and the limit price remain the hard bounds.
            return Ok(false);
        }

        let auction_start_price_offset =
            OrderParams::get_perp_baseline_start_price_offset(perp_market, self.direction)?;
        let new_auction_start_price = oracle_price.safe_add(auction_start_price_offset)?;

        if self.auction_duration.unwrap_or(0) == 0 {
            match self.direction {
                PositionDirection::Long => {
                    let ask_premium = perp_market
                        .amm
                        .last_ask_premium(&perp_market.market_stats)?;
                    let est_ask = oracle_price.safe_add(ask_premium)?.cast()?;

                    if self.price <= est_ask {
                        // if auction duration is empty and limit doesnt cross vamm premium, return early
                        return Ok(false);
                    } else {
                        let new_auction_start_price = new_auction_start_price.min(est_ask as i64);
                        msg!(
                            "Updating auction start price to {}",
                            new_auction_start_price
                        );
                        self.auction_start_price = Some(new_auction_start_price);
                        msg!("Updating auction end price to {}", self.price);
                        self.auction_end_price = Some(self.price as i64);
                    }
                }
                PositionDirection::Short => {
                    let bid_discount = perp_market
                        .amm
                        .last_bid_discount(&perp_market.market_stats)?;
                    let est_bid = oracle_price.safe_sub(bid_discount)?.cast()?;

                    if self.price >= est_bid {
                        // if auction duration is empty and limit doesnt cross vamm discount, return early
                        return Ok(false);
                    } else {
                        let new_auction_start_price = new_auction_start_price.max(est_bid as i64);
                        msg!(
                            "Updating auction start price to {}",
                            new_auction_start_price
                        );
                        self.auction_start_price = Some(new_auction_start_price);
                        msg!("Updating auction end price to {}", self.price);
                        self.auction_end_price = Some(self.price as i64);
                    }
                }
            }
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

        let auction_duration_before = self.auction_duration;
        let new_auction_duration = get_auction_duration(
            self.get_duration_floor_price_diff(auction_start_price, auction_end_price)?,
            oracle_price.unsigned_abs(),
            perp_market.contract_tier,
        )?;
        // ~4s of slop (in 400ms units) before overwriting a signed-msg duration
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

        Ok(auction_duration != self.auction_duration
            || auction_start_price != self.auction_start_price
            || auction_end_price != self.auction_end_price)
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
        let auction_duration = self.auction_duration;
        let auction_start_price = self.auction_start_price;
        let auction_end_price = self.auction_end_price;

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

        let auction_duration_before = self.auction_duration;
        let new_auction_duration = get_auction_duration(
            self.get_duration_floor_price_diff(auction_start_price, auction_end_price)?,
            oracle_price.unsigned_abs(),
            perp_market.contract_tier,
        )?;

        // ~4s of slop (in 400ms units) before overwriting a signed-msg duration
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

        Ok(auction_duration != self.auction_duration
            || auction_start_price != self.auction_start_price
            || auction_end_price != self.auction_end_price)
    }

    pub fn derive_market_order_auction_params(
        perp_market: &PerpMarket,
        direction: PositionDirection,
        oracle_price: i64,
        limit_price: u64,
        start_buffer: i64,
    ) -> VelocityResult<(i64, i64, u8)> {
        let (mut auction_start_price, mut auction_end_price) = if limit_price != 0 {
            let (auction_start_price_offset, auction_end_price_offset) =
                OrderParams::get_perp_baseline_start_end_price_offset(perp_market, direction, 2)?;
            let auction_start_price = oracle_price.safe_add(auction_start_price_offset)?;
            let auction_end_price = oracle_price.safe_add(auction_end_price_offset)?;

            (auction_start_price, auction_end_price)
        } else {
            let (auction_start_price_offset, auction_end_price_offset) =
                OrderParams::get_perp_baseline_start_end_price_offset(perp_market, direction, 1)?;
            let auction_start_price = oracle_price.safe_add(auction_start_price_offset)?;
            let auction_end_price = oracle_price.safe_add(auction_end_price_offset)?;

            (auction_start_price, auction_end_price)
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
        // Auction-param sanitization is OFF in test builds, deliberately: the
        // integration suites construct auctions on purpose that this would
        // rewrite, and rewriting them under the test's feet makes the fixtures
        // describe something other than what they assert on. It means a test
        // build accepts auction params production would sanitize — read test
        // results about auction pricing with that in mind.
        //
        // The bindings are consumed below in every other flavour; naming them
        // here keeps this from warning as an unreachable statement over
        // unused parameters, which reads like an abandoned edit rather than a
        // deliberate bypass.
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

    /// Widest distance from the oracle TWAP that a baseline auction start offset may sit
    /// (OtterSec #146).
    ///
    /// Reuses the tier band of `PerpMarket::get_auction_end_min_max_divisors`, which returns
    /// `(min_divisor, max_divisor)` for the auction END buffer. A larger divisor gives a smaller
    /// price, so `oracle_twap / max_divisor` is the widest auction the tier allows: 2% for tier A,
    /// 5% for B and C, 10% for Speculative, 20% for HighlySpeculative and Isolated. An auction that
    /// STARTS further from oracle than the widest auction the tier permits is nonsense, so that same
    /// number bounds the start offset.
    ///
    /// The bound is symmetric. A start offset is signed and can point either way, and the manipulated
    /// input can push it either way.
    ///
    /// A zero oracle TWAP gives a zero bound, so the offset collapses to zero. The END buffer clamp
    /// already degenerates the same way on such a market.
    fn get_perp_baseline_max_price_offset(perp_market: &PerpMarket) -> VelocityResult<i64> {
        let oracle_twap = perp_market
            .market_stats
            .historical_oracle_data
            .last_oracle_price_twap
            .unsigned_abs();
        let (_, max_divisor) = perp_market.get_auction_end_min_max_divisors()?;

        oracle_twap.safe_div(max_divisor)?.cast::<i64>()
    }

    /// Baseline auction start offset from the oracle price for a perp auction, per side.
    ///
    /// The result is clamped symmetrically to `get_perp_baseline_max_price_offset`. Both inputs are
    /// TWAPs a caller can influence: `update_perp_bid_ask_twap` samples the book from
    /// caller-supplied `User` accounts, and past 50bps of fast/slow divergence this function uses
    /// `last_mark_price_twap_5min` alone. These offsets set the auction band for a THIRD PARTY's
    /// forced close, so a moved TWAP prices a stranger's exit. `BID_ASK_TWAP_MIN_QUOTE_REST`
    /// raises the cost of moving those TWAPs; this clamp bounds the damage if one still moves.
    ///
    /// The sibling `get_perp_baseline_start_end_price_offset` clamps its END buffer to the same tier
    /// band, and derives the end offset with a `min`/`max` against the start offset, so clamping the
    /// start cannot invert start and end.
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

            // This branch divides a mark TWAP, not the oracle TWAP, so a mark far from oracle can
            // leave the tier band. Clamp it too.
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
        // ~32s in wall clock 400ms units
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

/// Network tag on a signed message: which cluster the taker signed for.
///
/// Without it, a message signed for devnet is byte-identical to one signed
/// for mainnet — the signature covers the order, not the chain it was meant
/// for — so a devnet order (cheap, farmable) could be replayed against
/// mainnet state. One byte closes that: `b'm'` / `b'd'`, checked against
/// the build's own cluster. `None` is the pre-tag encoding and stays
/// accepted (nothing is deployed to mainnet yet); once producers all emit
/// it, the check becomes mandatory.
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

/// Cap on a signed route (see [`SignedMsgOrderParamsMessage::route`]): a
/// message naming more custom quoters than this is refused rather than
/// silently truncated, which also keeps the message length bounded for
/// off-chain buffers.
///
/// It matches [`crate::instructions::MAX_ROUTE_QUOTERS`], because a
/// taker may name every entry one transaction can carry and naming more than
/// that could not be honoured. Raising this cap keeps every earlier message
/// valid: the field is a borsh `Vec`, so the wire format does not change.
pub const MAX_SIGNED_MSG_ROUTE_LEN: usize = crate::instructions::MAX_ROUTE_QUOTERS;

/// Digest of a signed route: the `QuoterV0` entries a taker chose, reduced to
/// the bytes a [`crate::state::signed_msg_user::SignedMsgOrderId`] can hold.
///
/// Canonicalised first (sorted, deduped) so the same choice always digests the
/// same way regardless of how a client ordered it, and an empty route digests
/// to zero — which is what makes one equality check cover both "no route was
/// signed" and "this is the route that was signed".
///
/// A route digest: the bytes an order stores to pin which quoter entries a
/// fill may claim its signer chose.
pub type RouteDigest = [u8; ROUTE_DIGEST_LEN];

/// Width of a [`RouteDigest`]. See [`route_digest`] for why it is this wide.
pub const ROUTE_DIGEST_LEN: usize = 8;

/// The digest of no route. A directly-placed order holds this, so one equality
/// check covers both "no route was signed" and "this is the signed route".
pub const NO_ROUTE_DIGEST: RouteDigest = [0; ROUTE_DIGEST_LEN];

/// Eight bytes, because a filler can enumerate candidates. It cannot choose
/// preimages freely: a claimed route must consist of entries the transaction
/// carries, and a fill carries at most `MAX_ROUTE_QUOTERS` of them. But it
/// picks which ones to carry, so its candidate set is every subset of that
/// size over the market's registered entries — public, stable, and worth
/// precomputing once. That count grows fast in the number of entries, which is
/// the number this design is trying to increase, so the width has to cover the
/// market this becomes and not the market it starts as.
///
/// A collision routes to an entry the taker did not pick, which is bounded by
/// the taker's own limit price rather than a theft. It still defeats the only
/// thing the digest is for.
///
/// The digest rides [`crate::state::signed_msg_user::SignedMsgOrderId`], whose
/// stride this width is chosen against. An earlier home on `Order` allowed only
/// five bytes, because `Order` is an array element in `User` and a sixth byte
/// would have changed that array's stride.
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
    // Never collide with "no route": a real route must be distinguishable
    // from an absent one.
    if out == [0; ROUTE_DIGEST_LEN] {
        out[0] = 1;
    }
    out
}

/// Trailing fields are appended, never inserted: the verifier zero-pads a
/// short payload before decoding, so an older producer's message reads as
/// `None` for everything it did not send (see
/// `validation::sig_verification`).
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
    /// The route the taker signed for: `QuoterV0` entries of the **custom**
    /// quoters (PropAMMs) it wants used. The CLOB and the vAMM are the
    /// mandatory baseline of every router fill, so they are implicit and
    /// never named here. Advisory to the program today — swift forwards it
    /// to keepers, which is what makes a routed order reach the quoters the
    /// taker chose.
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

    // duration granted per 1% of price diff; the step is the historical 400ms
    // calibration (100 or 60 of them per 1%, i.e. 40s / 24s per 1%)
    let steps_per_pct = if contract_tier.is_as_safe_as_contract(&ContractTier::B) {
        100
    } else {
        60
    };

    // `Order.auction_duration` stores wall clock 400ms units, not live slots,
    // so the value is independent of the slot duration and the u8 keeps the
    // full historical range (max 180 units = 72s; the ceiling is 255 = 102s).
    // Auction progress converts elapsed slots to wall clock through the
    // `SlotClock` at fill time.
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
    /// order. A maker that rests straight on the CLOB clears this: its CLOB
    /// placement record is the one statement about the order, and the ephemeral
    /// place record would be a redundant second one for the same resting order.
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
