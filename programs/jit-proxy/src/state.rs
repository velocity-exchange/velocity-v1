use anchor_lang::prelude::*;
use velocity::state::order_params::PostOnlyParam as VelocityPostOnlyParam;
use velocity::state::user::MarketType as VelocityMarketType;

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq)]
pub enum PostOnlyParam {
    None,
    MustPostOnly, // Tx fails if order can't be post only
    TryPostOnly,  // Tx succeeds and order not placed if can't be post only
    Slide,        // Modify price to be post only if can't be post only
}

impl PostOnlyParam {
    pub fn to_velocity_param(self) -> VelocityPostOnlyParam {
        match self {
            PostOnlyParam::None => VelocityPostOnlyParam::None,
            PostOnlyParam::MustPostOnly => VelocityPostOnlyParam::MustPostOnly,
            PostOnlyParam::TryPostOnly => VelocityPostOnlyParam::TryPostOnly,
            PostOnlyParam::Slide => VelocityPostOnlyParam::Slide,
        }
    }
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq)]
pub enum PriceType {
    Limit,
    Oracle,
}

#[derive(Clone, Copy, AnchorSerialize, AnchorDeserialize, PartialEq, Debug, Eq)]
pub enum MarketType {
    Perp,
    Spot,
}

impl MarketType {
    pub fn to_velocity_param(self) -> VelocityMarketType {
        match self {
            MarketType::Spot => VelocityMarketType::Spot,
            MarketType::Perp => VelocityMarketType::Perp,
        }
    }
}
