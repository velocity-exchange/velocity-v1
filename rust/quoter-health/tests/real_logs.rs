//! Attribution against logs a real program actually produced.
//!
//! The rest of this crate's tests use log lines written by hand, which check
//! the parser against what its author believed the format to be. These check
//! it against what velocity and the Solana runtime emit, captured from
//! `integration-tests/tests/router_fill.rs` running the real programs under
//! litesvm.
//!
//! Re-capture a fixture by running
//! `cargo test --locked a_reverting_quoter_leaves_only_its_cpi_frame` in
//! `integration-tests/`, which prints the logs between markers.

use {
    solana_sdk::pubkey::Pubkey,
    std::str::FromStr,
    velocity_quoter_health::{attribute, EntryRef, FailReason, RouteContext},
};

fn lines(name: &str) -> Vec<String> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {path}: {err}"))
        .lines()
        .map(str::to_string)
        .collect()
}

/// The CLOB program id these logs were captured against.
fn clob() -> Pubkey {
    Pubkey::from_str("BPX47ur8TbgZQgtJcGJvdcQMMFbmBP7ZrhpiUmLuHKqU").expect("valid id")
}

#[test]
fn a_reverting_quoter_is_resolved_from_its_cpi_frame() {
    // A failed CPI ends the calling instruction, so velocity never reaches
    // the line that would name the entry. The runtime's frame is all there
    // is, and it names a program rather than an entry.
    let entry = Pubkey::new_unique();
    let route = [EntryRef {
        quoter: entry,
        program: clob(),
    }];
    let verdict = attribute(
        &lines("quote_cpi_revert.log"),
        Some("InstructionError(0, InvalidInstructionData)"),
        RouteContext { entries: &route },
    );

    assert!(
        verdict.charges.is_empty(),
        "a frame names a program, not an entry, so it cannot convict on its own"
    );
    assert_eq!(
        verdict.suspects,
        vec![entry],
        "the frame must still point the router at something to drop"
    );
    assert!(verdict.unattributed.is_none());
}

#[test]
fn the_tenant_of_a_shared_program_is_resolved_by_frame_order() {
    // One quoter program serves many registry entries. These logs hold a
    // single frame, so the entry is the first on that program in the order
    // the route was built.
    let first = Pubkey::new_unique();
    let second = Pubkey::new_unique();
    let route = [
        EntryRef {
            quoter: first,
            program: clob(),
        },
        EntryRef {
            quoter: second,
            program: clob(),
        },
    ];
    let verdict = attribute(
        &lines("quote_cpi_revert.log"),
        None,
        RouteContext { entries: &route },
    );
    assert_eq!(verdict.suspects, vec![first]);
}

#[test]
fn a_quoter_that_was_not_on_the_route_is_never_suspected() {
    // With no entry on the failing program, the router has nothing to drop
    // and must charge the failure to itself.
    let route = [EntryRef {
        quoter: Pubkey::new_unique(),
        program: Pubkey::new_unique(),
    }];
    let verdict = attribute(
        &lines("quote_cpi_revert.log"),
        Some("InstructionError(0, InvalidInstructionData)"),
        RouteContext { entries: &route },
    );
    assert!(verdict.charges.is_empty());
    assert!(verdict.suspects.is_empty());
    assert_eq!(verdict.unattributed, Some(FailReason::Unknown));
}

#[test]
fn a_quoter_velocity_refused_is_charged_from_its_named_line() {
    // Velocity's own checks around the quoter call do run and do log, so
    // this half of the surface convicts on one line without a re-simulation.
    let quoter =
        Pubkey::from_str("GUbXazQwu6jx6kyp1M9KrSTqECjDkK5ACGiM4yTJheh9").expect("valid key");
    let verdict = attribute(
        &lines("quote_velocity_refusal.log"),
        Some("InstructionError(0, Custom(6129))"),
        RouteContext { entries: &[] },
    );
    assert_eq!(verdict.charges.len(), 1);
    assert_eq!(verdict.charges[0].quoter, quoter);
    assert!(verdict.charges[0].proof.is_actionable());
    assert!(verdict.suspects.is_empty());
    assert!(verdict.unattributed.is_none());
}

#[test]
fn an_anchor_error_is_read_from_the_debug_rendering_velocity_actually_writes() {
    // `Display for AnchorError` defers to `Debug`, so a `msg!("{}", e)`
    // writes `error_code_number: 6129`, not the prose `Error Number: 6129`
    // that `AnchorError::log` produces. Both appear in these logs, from
    // different call sites, and a parser that knows only the prose form
    // reads the wrong one.
    let logs = lines("quote_velocity_refusal.log");
    let named = logs
        .iter()
        .find(|line| line.contains("quote failed"))
        .expect("the named line");
    assert!(named.contains("error_code_number: 6129"));
    assert!(!named.contains("Error Number:"));
}

#[test]
fn the_routers_own_failure_is_not_charged_to_a_quoter() {
    // The same transaction error with no quoter named anywhere must not
    // convict the entry that happened to be on the route.
    let route = [EntryRef {
        quoter: Pubkey::new_unique(),
        program: Pubkey::new_unique(),
    }];
    let logs = vec![
        "Program vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P invoke [1]".to_string(),
        "Program log: Instruction: QuoteRouter".to_string(),
        "Program log: AnchorError occurred. Error Code: InsufficientCollateral. \
         Error Number: 6010. Error Message: Insufficient collateral."
            .to_string(),
    ];
    let verdict = attribute(
        &logs,
        Some("InstructionError(0, Custom(6010))"),
        RouteContext { entries: &route },
    );
    assert!(verdict.charges.is_empty());
    assert!(verdict.suspects.is_empty());
    assert!(verdict.unattributed.is_some());
}
