//! What a router saw a quoter do, and how sure it is that the quoter did it.
//!
//! A quoter fails most often during simulation. The router simulates before
//! it sends, so a quoter that reverts fails the simulation and the router
//! drops the transaction. Nothing lands. No log is archived and no event
//! fires. The only process that can see the failure is the one that held the
//! simulate call, which is why observations start here and not on chain.

use {
    serde::{Deserialize, Serialize},
    solana_sdk::pubkey::Pubkey,
};

/// Why a quoter's leg failed.
///
/// The velocity codes come from the program's `ErrorCode` enum. They are
/// contract violations: the quoter answered, and the answer broke a rule the
/// router checks. `Cpi` and `ComputeExhausted` mean the quoter never returned
/// a usable answer at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FailReason {
    /// The quoter's CPI returned an error.
    Cpi,
    /// The quoter consumed the transaction's remaining compute budget.
    ComputeExhausted,
    /// 6383. The quoter filled more base than the router allocated to it.
    Overfilled,
    /// 6384. The quoter filled at a price its own quote does not support.
    OffQuote,
    /// 6385. The quoter moved a user it may not act against.
    SubjectNotPermitted,
    /// 6382. The quoter's response did not decode.
    InvalidResponse,
    /// 6374 or 6375. The registry entry itself is wrong.
    Config,
    /// The failure is a quoter's, but the code is not one this crate knows.
    Unknown,
}

impl FailReason {
    /// Map a velocity anchor error code onto a reason.
    ///
    /// Codes outside the quoter range return `None`. That distinction
    /// matters: a failure the router caused must never be charged to a
    /// quoter.
    pub fn from_velocity_code(code: u32) -> Option<Self> {
        match code {
            6374 | 6375 => Some(Self::Config),
            6382 => Some(Self::InvalidResponse),
            6383 => Some(Self::Overfilled),
            6384 => Some(Self::OffQuote),
            6385 => Some(Self::SubjectNotPermitted),
            _ => None,
        }
    }

    /// True when the reason is a broken promise rather than bad luck.
    ///
    /// A quoter that answers off its own quote, overfills, or moves a user it
    /// does not own has violated the response contract. One of these is worth
    /// more than many plain reverts, so the policy weighs them apart.
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
/// Attribution must be positive. A simulation that carries several quoters
/// fails for many reasons that belong to nobody: the taker's own margin, a
/// stale oracle, an account the builder left out. A router that blames a
/// quoter by default charges makers for its own bugs, so an unproven failure
/// stays unattributed and is counted against the router instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Attribution {
    /// Velocity named the entry in its own log line. Available for answers
    /// velocity refused, not for quoters whose CPI reverted: a failed CPI
    /// ends the calling instruction before velocity can log.
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

    /// True when the evidence is strong enough to move a quoter's state.
    ///
    /// A named line and a re-simulation both identify one entry. A CPI
    /// bracket only identifies one program, and it resolves to an entry
    /// through the route's order. That inference breaks if the on-chain entry
    /// set moved after the route was built, so a bracket stays a hypothesis.
    /// Re-simulate without the suspect to settle it.
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

/// A failure no quoter was proven to have caused.
///
/// This is a measure of the router's own blind spot. A rising count means
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
