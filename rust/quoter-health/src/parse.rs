//! Turn the logs of a failed simulation into an attributed cause.
//!
//! A simulation that carries several quoters fails for many reasons that
//! belong to no quoter. The taker's own margin, a stale oracle, a compute
//! limit, and an account the builder left out are four of them. A quoter is
//! charged only when the evidence names it. Everything else counts as
//! unattributed, against the router.
//!
//! Two kinds of evidence appear in the logs, and they split the failure
//! surface between them.
//!
//! Velocity names the entry for an answer it refuses. Every message velocity
//! writes about a quoter it could not use starts `quoter <key>`. That covers
//! the checks around the quote call, the execute leg, and each fill-path
//! response check, which is where the contract violations appear. One named
//! line is enough to charge a quoter.
//!
//! Who failed and why arrive separately. `validate!` writes the error on one
//! line and the message on the next. The reason therefore comes from the
//! transaction's own error code, which is the code that aborted it.
//!
//! The runtime names the program for a quoter that never answers. A failed
//! CPI ends the calling instruction, so velocity never reaches the line where
//! it would name the entry. A quoter that reverts, or that exhausts the
//! compute budget, leaves no named line at all. Only the runtime's
//! `Program <id> invoke [2]` frame and its failure survive. Captured logs in
//! `tests/real_logs.rs` pin this. Do not assume that a revert names itself.
//!
//! A program id is not an entry, because one quoter program serves many
//! registry entries. Counting frames against the route's entry order resolves
//! which entry it was, but only if the on-chain entry set still matched the
//! one the route was built from. A frame therefore yields a suspect, never a
//! charge. A re-simulation without that suspect turns it into proof.

use {
    crate::observe::{Attribution, FailReason},
    solana_sdk::pubkey::Pubkey,
    std::str::FromStr,
};

/// A registry entry carried by the simulated instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryRef {
    pub quoter: Pubkey,
    pub program: Pubkey,
}

/// The route a simulation was built from.
///
/// `entries` must be in the order the builder placed them in the account
/// tail. The on-chain router walks them in that order, so the order is what
/// lets a CPI bracket be matched to an entry.
#[derive(Clone, Copy, Debug)]
pub struct RouteContext<'a> {
    pub entries: &'a [EntryRef],
}

/// One quoter charged with one failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Charge {
    pub quoter: Pubkey,
    pub reason: FailReason,
    pub proof: Attribution,
}

/// What the logs of one failed simulation say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Verdict {
    /// Quoters the evidence names outright.
    pub charges: Vec<Charge>,
    /// Quoters the CPI brackets point at. These are hypotheses. Re-simulate
    /// without them to decide.
    pub suspects: Vec<Pubkey>,
    /// Set when nothing in the logs points at any quoter.
    pub unattributed: Option<FailReason>,
}

impl Verdict {
    /// True when no quoter was named and no quoter is suspected.
    pub fn is_empty(&self) -> bool {
        self.charges.is_empty() && self.suspects.is_empty()
    }
}

const LOG_PREFIX: &str = "Program log: ";
const NAMED_MARKER: &str = "quoter ";

/// Substrings the runtime uses when a program exhausts the compute budget.
/// The wording changed across runtime versions, so match several.
const COMPUTE_EXHAUSTED_MARKERS: [&str; 4] = [
    "exceeded CUs meter",
    "Computational budget exceeded",
    "exceeded maximum number of instructions",
    "compute budget exceeded",
];

fn is_compute_exhausted(line: &str) -> bool {
    COMPUTE_EXHAUSTED_MARKERS
        .iter()
        .any(|marker| line.contains(marker))
}

/// Read the code from either anchor rendering of an error.
///
/// `Debug` writes `error_code_number: 6385`. The prose form that
/// `AnchorError::log` writes carries `Error Number: 6385`. Both appear in
/// logs, from different call sites, so both are read.
pub fn anchor_error_number(text: &str) -> Option<u32> {
    let rest = text
        .split("error_code_number:")
        .nth(1)
        .or_else(|| text.split("Error Number:").nth(1))?;
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Read the code from `Custom program error: 0x18f0`. A failed CPI reaches
/// the caller in that form.
fn custom_program_error(text: &str) -> Option<u32> {
    let rest = text
        .split("ustom program error:")
        .nth(1)?
        .trim_start()
        .strip_prefix("0x")?;
    let hex: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    u32::from_str_radix(&hex, 16).ok()
}

/// The reason a named line reports, from the error text that follows the key.
///
/// Anchor renders an `Error` through `Debug`, not through the prose form its
/// `log()` method writes. The text therefore carries
/// `error_code_number: 6385` and `error_name: "QuoterFillOffQuote"` rather
/// than `Error Number: 6385`. A failed CPI arrives as a plain `ProgramError`,
/// which renders as `Custom program error: 0x18f0`. All three shapes are read
/// here.
fn reason_from_text(text: &str) -> FailReason {
    if let Some(reason) = anchor_error_number(text).and_then(FailReason::from_velocity_code) {
        return reason;
    }
    if let Some(reason) = custom_program_error(text).and_then(FailReason::from_velocity_code) {
        return reason;
    }

    // Anchor renders the variant name as well as the number, and the
    // program's own `validate!` messages carry only the name.
    for (name, reason) in [
        ("QuoterOverfilled", FailReason::Overfilled),
        ("QuoterFillOffQuote", FailReason::OffQuote),
        ("QuoterSubjectNotPermitted", FailReason::SubjectNotPermitted),
        ("InvalidQuoterResponse", FailReason::InvalidResponse),
        ("InvalidQuoterConfig", FailReason::Config),
        ("InvalidQuoterAuthority", FailReason::Config),
    ] {
        if text.contains(name) {
            return reason;
        }
    }

    if is_compute_exhausted(text) {
        return FailReason::ComputeExhausted;
    }

    FailReason::Cpi
}

/// Read `quoter <key> <what happened>` out of one log line. Every velocity
/// message about an unusable quoter opens this way, so the shape after it
/// is not constrained. The key filters a maker's own log starting with the
/// same word: an arbitrary word does not parse as a base58 public key.
fn parse_named(line: &str) -> Option<(Pubkey, FailReason)> {
    let body = line.strip_prefix(LOG_PREFIX)?;
    let rest = body.strip_prefix(NAMED_MARKER)?;
    let (key, tail) = rest.split_once(' ')?;
    let quoter = Pubkey::from_str(key).ok()?;
    Some((quoter, reason_from_text(tail)))
}

/// Read the program id out of a runtime `Program <id> invoke [N]` line.
///
/// Depth 1 is the router itself. Only deeper frames are quoter CPIs.
fn parse_invoke(line: &str) -> Option<Pubkey> {
    let rest = line.strip_prefix("Program ")?;
    let (id, tail) = rest.split_once(' ')?;
    let depth = tail.strip_prefix("invoke [")?.strip_suffix(']')?;
    if depth.parse::<u32>().ok()? < 2 {
        return None;
    }

    Pubkey::from_str(id).ok()
}

/// Attribute a failed simulation.
///
/// `err` is the transaction error the simulation reported. It is used only to
/// classify a failure the logs do not otherwise explain.
pub fn attribute(logs: &[String], err: Option<&str>, route: RouteContext<'_>) -> Verdict {
    let mut verdict = Verdict::default();

    // `validate!` writes the error on one line and the message naming the
    // quoter on the next, so a named line often carries no reason at all. The
    // transaction's own error code is the code that aborted, so it is the
    // better answer wherever it names a quoter failure.
    let from_err = err
        .and_then(velocity_code_from_err)
        .and_then(FailReason::from_velocity_code);

    for line in logs {
        if let Some((quoter, reason)) = parse_named(line) {
            // The same quoter can fail both legs in one simulation. Keep the
            // first reason: it is the one that stopped the router.
            if !verdict.charges.iter().any(|c| c.quoter == quoter) {
                verdict.charges.push(Charge {
                    quoter,
                    reason: from_err.unwrap_or(reason),
                    proof: Attribution::Named,
                });
            }
        }
    }

    if !verdict.charges.is_empty() {
        return verdict;
    }

    // Nothing named a quoter. Fall back to the CPI brackets, counting each
    // program's invocations so a multi-tenant program resolves to the entry
    // the router was walking when it died.
    let mut invoked: Vec<Pubkey> = Vec::new();
    let mut exhausted = err.map(is_compute_exhausted).unwrap_or(false);
    for line in logs {
        if let Some(program) = parse_invoke(line) {
            invoked.push(program);
        }
        if is_compute_exhausted(line) {
            exhausted = true;
        }
    }

    if let Some(last) = invoked.last() {
        let nth = invoked.iter().filter(|program| *program == last).count() - 1;
        let suspect = route
            .entries
            .iter()
            .filter(|entry| entry.program == *last)
            .nth(nth)
            .or_else(|| route.entries.iter().find(|entry| entry.program == *last));
        if let Some(entry) = suspect {
            verdict.suspects.push(entry.quoter);
            return verdict;
        }
    }

    verdict.unattributed = Some(if exhausted {
        FailReason::ComputeExhausted
    } else {
        err.and_then(velocity_code_from_err)
            .and_then(FailReason::from_velocity_code)
            .unwrap_or(FailReason::Unknown)
    });

    verdict
}

/// Read `Custom(NNNN)` out of a transaction error rendering.
fn velocity_code_from_err(err: &str) -> Option<u32> {
    let rest = err.split("Custom(").nth(1)?;
    rest.split(')').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    fn route(entries: &[EntryRef]) -> RouteContext<'_> {
        RouteContext { entries }
    }

    #[test]
    fn the_programs_own_line_names_the_quoter() {
        let quoter = key(7);
        let logs = vec![
            "Program log: Instruction: QuoteRouter".to_string(),
            format!("Program log: quoter {quoter} quote failed: some failure"),
        ];
        let verdict = attribute(&logs, Some("InstructionError(0, Custom(6000))"), route(&[]));
        assert_eq!(
            verdict.charges,
            vec![Charge {
                quoter,
                reason: FailReason::Cpi,
                proof: Attribution::Named,
            }]
        );

        assert!(verdict.unattributed.is_none());
    }

    #[test]
    fn a_named_line_carries_the_contract_violation_it_reports() {
        let quoter = key(9);
        let logs = vec![format!(
            "Program log: quoter {quoter} execute failed: AnchorError occurred. \
             Error Code: QuoterFillOffQuote. Error Number: 6385. Error Message: x."
        )];
        let verdict = attribute(&logs, None, route(&[]));
        assert_eq!(verdict.charges[0].reason, FailReason::OffQuote);
        assert!(verdict.charges[0].reason.is_contract_violation());
    }

    #[test]
    fn a_failure_that_names_nobody_is_charged_to_nobody() {
        let logs = vec![
            "Program log: Instruction: QuoteRouter".to_string(),
            "Program log: AnchorError occurred. Error Code: InsufficientCollateral. \
             Error Number: 6010. Error Message: x."
                .to_string(),
        ];
        let verdict = attribute(&logs, Some("InstructionError(0, Custom(6010))"), route(&[]));
        assert!(verdict.charges.is_empty());
        assert!(verdict.suspects.is_empty());
        assert_eq!(verdict.unattributed, Some(FailReason::Unknown));
    }

    #[test]
    fn compute_exhaustion_leaves_a_suspect_not_a_charge() {
        // A quoter that burns the budget leaves the router no room to log,
        // so only the runtime's brackets remain.
        let program = key(3);
        let entries = [EntryRef {
            quoter: key(4),
            program,
        }];
        let logs = vec![
            format!("Program {program} invoke [2]"),
            "Program failed to complete: exceeded CUs meter at BPF instruction".to_string(),
        ];
        let verdict = attribute(&logs, None, route(&entries));
        assert!(verdict.charges.is_empty());
        assert_eq!(verdict.suspects, vec![key(4)]);
    }

    #[test]
    fn brackets_resolve_the_tenant_of_a_shared_program() {
        // One quoter program serves many registry entries. The second
        // invocation of that program is the second entry on it.
        let program = key(3);
        let entries = [
            EntryRef {
                quoter: key(10),
                program,
            },
            EntryRef {
                quoter: key(11),
                program,
            },
            EntryRef {
                quoter: key(12),
                program,
            },
        ];
        let logs = vec![
            format!("Program {program} invoke [2]"),
            format!("Program {program} success"),
            format!("Program {program} invoke [2]"),
            "Program failed to complete: Computational budget exceeded".to_string(),
        ];
        let verdict = attribute(&logs, None, route(&entries));
        assert_eq!(verdict.suspects, vec![key(11)]);
    }

    #[test]
    fn the_routers_own_frame_is_never_a_suspect() {
        let router = key(1);
        let logs = vec![format!("Program {router} invoke [1]")];
        let verdict = attribute(&logs, Some("InstructionError(0, Custom(6010))"), route(&[]));
        assert!(verdict.suspects.is_empty());
        assert!(verdict.unattributed.is_some());
    }

    #[test]
    fn a_named_line_wins_over_the_brackets() {
        let program = key(3);
        let quoter = key(4);
        let entries = [EntryRef {
            quoter: key(99),
            program,
        }];
        let logs = vec![
            format!("Program {program} invoke [2]"),
            format!("Program log: quoter {quoter} quote failed: boom"),
        ];
        let verdict = attribute(&logs, None, route(&entries));
        assert_eq!(verdict.charges[0].quoter, quoter);
        assert!(verdict.suspects.is_empty());
    }

    #[test]
    fn a_maker_log_that_starts_with_the_same_word_is_not_a_charge() {
        let logs = vec!["Program log: quoter set mid to 100".to_string()];
        let verdict = attribute(&logs, None, route(&[]));
        assert!(verdict.charges.is_empty());
    }

    #[test]
    fn only_proven_attribution_is_actionable() {
        assert!(Attribution::Named.is_actionable());
        assert!(Attribution::Resim.is_actionable());
        assert!(Attribution::Bisect.is_actionable());
        assert!(!Attribution::Bracketed.is_actionable());
    }

    #[test]
    fn a_fill_path_response_check_names_its_quoter() {
        // `validate!` writes the error on one line and the message on the
        // next, so the reason comes from the transaction's error code and
        // the key from the message. Neither line carries both.
        let quoter = key(11);
        let logs = vec![
            "Program log: Error Quoter filled at a price its quote does not \
             support thrown at programs/velocity/src/controller/orders.rs:4315"
                .to_string(),
            format!("Program log: quoter {quoter} filled 100/5 off its quote of 400"),
        ];
        let verdict = attribute(&logs, Some("InstructionError(0, Custom(6385))"), route(&[]));
        assert_eq!(
            verdict.charges,
            vec![Charge {
                quoter,
                reason: FailReason::OffQuote,
                proof: Attribution::Named,
            }]
        );
    }

    #[test]
    fn a_subject_violation_is_read_off_the_transaction_code() {
        let quoter = key(12);
        let logs = vec![format!(
            "Program log: quoter {quoter} may not act against user {}",
            key(13)
        )];
        let verdict = attribute(&logs, Some("InstructionError(0, Custom(6386))"), route(&[]));
        assert_eq!(verdict.charges[0].reason, FailReason::SubjectNotPermitted);
        assert!(verdict.charges[0].reason.is_contract_violation());
    }

    #[test]
    fn an_anchor_error_rendered_by_debug_still_yields_its_code() {
        // `Display for AnchorError` defers to `Debug`, so the prose form
        // `AnchorError::log` writes is not what a `msg!("{}", e)` produces.
        let quoter = key(14);
        let logs = vec![format!(
            "Program log: quoter {quoter} execute failed: AnchorError {{ \
             error_name: \"QuoterOverfilled\", error_code_number: 6384, \
             error_msg: \"...\", error_origin: None, compared_values: None }}"
        )];
        let verdict = attribute(&logs, None, route(&[]));
        assert_eq!(verdict.charges[0].reason, FailReason::Overfilled);
    }

    #[test]
    fn a_failed_cpi_reads_as_a_revert_not_a_contract_violation() {
        // A quoter whose CPI errors reaches velocity as a plain
        // `ProgramError`, which renders as a hex custom code.
        let quoter = key(15);
        let logs = vec![format!(
            "Program log: quoter {quoter} quote failed: Custom program error: 0x1770"
        )];
        let verdict = attribute(&logs, Some("InstructionError(0, Custom(6000))"), route(&[]));
        assert_eq!(verdict.charges[0].reason, FailReason::Cpi);
        assert!(!verdict.charges[0].reason.is_contract_violation());
    }

    #[test]
    fn a_quoters_own_hex_error_maps_when_it_is_one_velocity_knows() {
        let quoter = key(16);
        // 0x18f0 == 6384, QuoterOverfilled.
        let logs = vec![format!(
            "Program log: quoter {quoter} execute failed: Custom program error: 0x18f0"
        )];
        let verdict = attribute(&logs, None, route(&[]));
        assert_eq!(verdict.charges[0].reason, FailReason::Overfilled);
    }
}
