//! What a router saw a quoter do, and how sure it is that the quoter did it.
//!
//! A quoter fails most often during simulation. The router simulates before
//! it sends, so a quoter that reverts fails the simulation and the router
//! drops the transaction. Nothing lands. No log is archived and no event
//! fires. Only the process that held the simulate call can see the failure,
//! so observation starts there and not on chain.

use {
    serde::{Deserialize, Serialize},
    solana_sdk::pubkey::Pubkey,
};

/// Why a quoter's leg failed.
///
/// The velocity codes come from the program's `ErrorCode` enum. They are
/// contract violations. The quoter answered, and the answer broke a rule the
/// router checks. `Cpi` and `ComputeExhausted` mean the quoter never returned
/// a usable answer at all.
///
/// A code follows the variant's position in that enum, so a variant added
/// above one of these renumbers it. This crate takes no velocity dependency,
/// so the numbers below are a copy. `velocity-router-sim`'s `fail_reason_pin`
/// tests hold the copy to the enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FailReason {
    /// The quoter's CPI returned an error.
    Cpi,
    /// The quoter consumed the transaction's remaining compute budget.
    ComputeExhausted,
    /// 6384. The quoter filled more base than the router allocated to it.
    Overfilled,
    /// 6385. The quoter filled at a price its own quote does not support.
    OffQuote,
    /// 6386. The quoter moved a user it may not act against.
    SubjectNotPermitted,
    /// 6383. The quoter's response did not decode.
    InvalidResponse,
    /// 6375 or 6376. The registry entry itself is wrong.
    Config,
    /// The failure is a quoter's, but the code is not one this crate knows.
    Unknown,
}

impl FailReason {
    /// Map a velocity anchor error code onto a reason.
    ///
    /// Codes outside the quoter range return `None`. A failure the router
    /// caused must never be charged to a quoter.
    pub fn from_velocity_code(code: u32) -> Option<Self> {
        match code {
            6375 | 6376 => Some(Self::Config),
            6383 => Some(Self::InvalidResponse),
            6384 => Some(Self::Overfilled),
            6385 => Some(Self::OffQuote),
            6386 => Some(Self::SubjectNotPermitted),
            _ => None,
        }
    }

    /// True when the quoter broke the response contract. A quoter that
    /// answers off its own quote, overfills, or moves a user it does not
    /// own broke the contract. The policy weighs this heavier than a plain
    /// revert.
    pub fn is_contract_violation(self) -> bool {
        matches!(
            self,
            Self::Overfilled | Self::OffQuote | Self::SubjectNotPermitted | Self::InvalidResponse
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpi => "cpi",
            Self::ComputeExhausted => "compute_exhausted",
            Self::Overfilled => "overfilled",
            Self::OffQuote => "off_quote",
            Self::SubjectNotPermitted => "subject_not_permitted",
            Self::InvalidResponse => "invalid_response",
            Self::Config => "config",
            Self::Unknown => "unknown",
        }
    }
}

/// How the router decided which quoter caused a failure.
///
/// A simulation carries several quoters, and it fails for many reasons that
/// belong to no quoter. The taker's own margin, a stale oracle, and an
/// account the builder left out are three of them. A router that blames a
/// quoter by default charges makers for its own bugs. An unproven failure
/// therefore stays unattributed and counts against the router.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Attribution {
    /// Velocity named the entry in its own log line. A failed CPI ends the
    /// calling instruction before velocity can log, so this covers only the
    /// answers velocity refused.
    Named,
    /// The runtime's CPI brackets named the program, and the route's entry
    /// order resolved which entry on that program it was.
    Bracketed,
    /// The simulation passed once the suspect was excluded.
    Resim,
    /// A bisection over the entry set isolated it.
    Bisect,
}

impl Attribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Named => "named",
            Self::Bracketed => "bracketed",
            Self::Resim => "resim",
            Self::Bisect => "bisect",
        }
    }

    /// True when the evidence is strong enough to move a quoter's state. A
    /// named line or a re-simulation identifies one entry directly. A CPI
    /// bracket identifies only the program, resolved to an entry through
    /// the route's order, so it stays a hypothesis if that order later shifts.
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::Named | Self::Resim | Self::Bisect)
    }
}

/// One thing a router saw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Observation {
    /// The quoter took part in a simulation that succeeded.
    SimOk { cu: u64 },
    /// The quoter caused a simulation to fail.
    SimFail {
        reason: FailReason,
        proof: Attribution,
    },

    /// The quoter's execute leg settled.
    ExecuteOk {
        allocated_base: u64,
        filled_base: u64,
    },

    /// The quoter's execute leg failed in a landed or simulated fill.
    ExecuteFail { reason: FailReason },
    /// The router cut the quoter's ladder before quoting it. A Custom entry
    /// is clamped to what its user's margin supports, so a large clamp means
    /// the quoter offered depth it cannot carry.
    DepthClamped {
        quoted_base: u64,
        admitted_base: u64,
    },

    /// A route that named this quoter landed. The prices are the one the
    /// router published off chain and the one the fill executed at.
    RouteLanded {
        quoted_price: u64,
        executed_price: u64,
        taker_long: bool,
    },
}

impl Observation {
    /// How much worse than the published quote the fill executed, in basis
    /// points of the quoted price. A positive value means the taker did worse
    /// than the route promised. `None` for any other observation, and for a
    /// quote of zero, which has no scale to measure against.
    pub fn adverse_slip_bps(self) -> Option<f64> {
        let Self::RouteLanded {
            quoted_price,
            executed_price,
            taker_long,
        } = self
        else {
            return None;
        };

        if quoted_price == 0 {
            return None;
        }

        let delta = executed_price as f64 - quoted_price as f64;
        let adverse = if taker_long { delta } else { -delta };

        Some(adverse / quoted_price as f64 * 10_000.0)
    }
}

/// A failure no quoter was proven to have caused.
///
/// This measures what the router failed to attribute. A rising count means
/// attribution has a hole, not that makers got worse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Unattributed {
    pub reason: FailReason,
    pub market: u16,
}

/// An observation together with who it is about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Report {
    pub quoter: Pubkey,
    pub market: u16,
    pub observation: Observation,
}

impl Report {
    pub fn new(quoter: Pubkey, market: u16, observation: Observation) -> Self {
        Self {
            quoter,
            market,
            observation,
        }
    }
}
