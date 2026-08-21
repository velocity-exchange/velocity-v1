//! Quote a market without letting one quoter silence it.
//!
//! `quote_router` CPIs every registry entry the instruction carries. One
//! entry that reverts fails the whole simulation, so today a single bad
//! quoter stops a market's book from publishing and turns a route request
//! into an error. The market's other sources were fine; nothing reached them.
//!
//! This module closes that. When a simulation fails it reads the logs, works
//! out which entry caused it, drops that entry, and simulates again. The
//! caller gets the market minus the bad quoter instead of nothing.
//!
//! The retry is also the measurement. A log line naming a quoter is strong
//! evidence, but a simulation that passes once one entry is removed is proof,
//! and it costs nothing extra because the retry is the path the router wants
//! to take anyway. Proof is only claimed when exactly one entry was removed:
//! dropping two at once and succeeding shows that at least one was at fault,
//! not which.

use {
    crate::quote_view::{
        build_quote_router_ix, simulate_quote_view_with_cost, CarriedEntry, QuoteRouterParams,
        QuoteSimFailure, QuoteView,
    },
    anyhow::Result,
    relay_chain_source::ChainSource,
    solana_sdk::pubkey::Pubkey,
    velocity_quoter_health::{
        attribute,
        observe::{Attribution, Observation, Report},
        EntryRef, Health, RouteContext,
    },
};

/// The upgradeable loader. A program's deploy record lives in a PDA it owns.
const UPGRADEABLE_LOADER: Pubkey =
    solana_sdk::pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");

/// `UpgradeableLoaderState::ProgramData` puts the last deploy slot straight
/// after a four-byte enum tag.
const DEPLOY_SLOT_OFFSET: usize = 4;

/// Tell the health layer when a quoter's program was last deployed.
///
/// A redeploy makes the score describe code that is no longer running, so the
/// counters are dropped and the quoter goes on probation rather than keeping
/// full flow. That is the broken-rollout case: degradation and restoration
/// both happen without an operator. A program serving many registry entries
/// moves all of them, which is right — the upgrade changed every tenant.
///
/// A program that is not upgradeable has no deploy record, and is skipped:
/// its code cannot change, so its score cannot go stale.
pub async fn watch_program_deploys<S: ChainSource>(
    source: &S,
    health: &Health,
    entries: &[EntryRef],
) -> Result<()> {
    let mut seen: Vec<(Pubkey, Pubkey)> = entries
        .iter()
        .map(|entry| {
            let (data, _) =
                Pubkey::find_program_address(&[entry.program.as_ref()], &UPGRADEABLE_LOADER);
            (entry.quoter, data)
        })
        .collect();
    seen.dedup_by_key(|(_, data)| *data);
    if seen.is_empty() {
        return Ok(());
    }
    let keys: Vec<Pubkey> = seen.iter().map(|(_, data)| *data).collect();
    let accounts = source.get_multiple_accounts(&keys).await?;
    for ((quoter, _), account) in seen.iter().zip(accounts) {
        let Some(account) = account else { continue };
        let Some(bytes) = account.data.get(DEPLOY_SLOT_OFFSET..DEPLOY_SLOT_OFFSET + 8) else {
            continue;
        };
        let slot = u64::from_le_bytes(bytes.try_into().expect("eight bytes"));
        health.observe_program_slot(quoter, slot);
    }
    Ok(())
}

/// How many times a market may be re-quoted with more entries removed.
///
/// Each round removes at least one entry, so this bounds both the work and
/// how much of a market a single request may strip away.
const MAX_ATTEMPTS: usize = 3;

/// What a market quoted, and what it cost to get there.
pub struct HealthyQuote {
    pub view: QuoteView,
    pub units_consumed: u64,
    /// Entries left out of the simulation that produced this view.
    pub excluded: Vec<Pubkey>,
    /// Entries the simulation carried, in the order the router walked them.
    pub entries: Vec<CarriedEntry>,
}

/// Everything `build_quote_router_ix` needs, gathered so the retry loop can
/// rebuild without the caller restating it.
#[derive(Clone, Copy)]
pub struct QuoteRequest<'a> {
    pub velocity: Pubkey,
    pub authority: Pubkey,
    pub quote_buffer: Pubkey,
    pub market_index: u16,
    pub direction: program::state::prop_amm::Direction,
    pub size: u64,
    pub dlob_makers: &'a [Pubkey],
    /// Carry only these entries. `None` carries every live entry, which only
    /// fits while a market has few enough of them.
    pub only: Option<&'a [Pubkey]>,
    /// Quote the vAMM into this pass.
    pub include_vamm: bool,
}

impl<'a> QuoteRequest<'a> {
    /// Every field a market needs quoting in one pass.
    pub fn whole_market(
        velocity: Pubkey,
        authority: Pubkey,
        quote_buffer: Pubkey,
        market_index: u16,
        direction: program::state::prop_amm::Direction,
        size: u64,
        dlob_makers: &'a [Pubkey],
    ) -> Self {
        Self {
            velocity,
            authority,
            quote_buffer,
            market_index,
            direction,
            size,
            dlob_makers,
            only: None,
            include_vamm: true,
        }
    }

    fn params<'b>(&'b self, exclude: &'b [Pubkey]) -> QuoteRouterParams<'b> {
        QuoteRouterParams {
            velocity: &self.velocity,
            authority: &self.authority,
            quote_buffer: &self.quote_buffer,
            market_index: self.market_index,
            direction: self.direction,
            size: self.size,
            dlob_makers: self.dlob_makers,
            exclude,
            only: self.only,
            include_vamm: self.include_vamm,
        }
    }
}

/// Quote a market, routing around quoters that break the simulation.
pub async fn quote_with_health<S: ChainSource>(
    source: &S,
    health: &Health,
    request: &QuoteRequest<'_>,
) -> Result<HealthyQuote> {
    // Quarantined and denied entries never enter a simulation. Everything
    // below is about entries that were admitted and then failed anyway.
    let mut excluded = health.excluded();
    let mut last_dropped: Vec<Pubkey> = Vec::new();
    let mut last_reason = None;
    let mut error = None;

    for _ in 0..MAX_ATTEMPTS {
        let built = build_quote_router_ix(source, &request.params(&excluded)).await?;

        // Throttled quoters ride a fraction of routes. Deciding it here, on
        // the entries this market actually holds, keeps one market's traffic
        // from spending another market's sampling budget.
        let sampled_out: Vec<Pubkey> = built
            .entries
            .iter()
            .map(|entry| entry.quoter)
            .filter(|quoter| !health.admits(quoter))
            .collect();
        let built = if sampled_out.is_empty() {
            built
        } else {
            excluded.extend(sampled_out);
            build_quote_router_ix(source, &request.params(&excluded)).await?
        };

        match simulate_quote_view_with_cost(
            source,
            built.instruction,
            &request.authority,
            &request.quote_buffer,
        )
        .await
        {
            Ok((view, units_consumed)) => {
                // Removing exactly one entry and succeeding proves that entry
                // caused the failure. Removing several proves only that one of
                // them did, so the others keep the benefit of the doubt.
                if let (1, Some(reason)) = (last_dropped.len(), last_reason) {
                    health.record(Report::new(
                        last_dropped[0],
                        request.market_index,
                        Observation::SimFail {
                            reason,
                            proof: Attribution::Resim,
                        },
                    ));
                }
                for entry in &built.entries {
                    health.record(Report::new(
                        entry.quoter,
                        request.market_index,
                        Observation::SimOk { cu: units_consumed },
                    ));
                }
                // The view reports whether margin verification cut a book
                // below what its source quoted. It reports that a cut
                // happened, not how deep, so this counts the share of quotes
                // a quoter had cut rather than the share of base lost. Both
                // answer the same question: is this quoter offering depth
                // the account behind it cannot carry.
                for book in &view.books {
                    if built.entries.iter().any(|entry| entry.quoter == book.key) {
                        health.record(Report::new(
                            book.key,
                            request.market_index,
                            Observation::DepthClamped {
                                quoted_base: 1,
                                admitted_base: u64::from(!book.clamped),
                            },
                        ));
                    }
                }
                return Ok(HealthyQuote {
                    view,
                    units_consumed,
                    excluded,
                    entries: built.entries,
                });
            }
            Err(err) => {
                let Some(failure) = err.downcast_ref::<QuoteSimFailure>() else {
                    // Not a simulation failure. Nothing here is a quoter's.
                    return Err(err);
                };
                let entry_refs: Vec<EntryRef> = built
                    .entries
                    .iter()
                    .map(CarriedEntry::as_entry_ref)
                    .collect();
                let verdict = attribute(
                    &failure.logs,
                    Some(&failure.err),
                    RouteContext {
                        entries: &entry_refs,
                    },
                );
                health.record_verdict(request.market_index, &verdict);

                let round = plan_retry(&verdict, &excluded);
                last_reason = round.reason;
                last_dropped = round.drop;

                if last_dropped.is_empty() {
                    // The logs point at no quoter, so removing one would be a
                    // guess. Hand the failure back rather than quietly
                    // stripping the market down.
                    return Err(err);
                }
                excluded.extend(last_dropped.iter().copied());
                error = Some(err);
            }
        }
    }

    Err(error.unwrap_or_else(|| anyhow::anyhow!("quote_router did not settle")))
}

/// What one failed round says to do next.
struct Retry {
    /// Entries to leave out of the next simulation.
    drop: Vec<Pubkey>,
    /// The failure to charge them with, once a retry proves it.
    reason: Option<velocity_quoter_health::FailReason>,
}

/// Decide what to remove after a failed round.
///
/// Charges and suspects both get dropped: a charge is named by the program,
/// a suspect is inferred from the CPI brackets, and removing either is how a
/// suspect becomes proof. Entries already excluded are not re-listed, so a
/// round that names only what is already gone drops nothing and ends the
/// loop rather than spinning.
fn plan_retry(verdict: &velocity_quoter_health::Verdict, excluded: &[Pubkey]) -> Retry {
    Retry {
        drop: verdict
            .charges
            .iter()
            .map(|charge| charge.quoter)
            .chain(verdict.suspects.iter().copied())
            .filter(|quoter| !excluded.contains(quoter))
            .collect(),
        reason: verdict
            .charges
            .first()
            .map(|charge| charge.reason)
            .or(verdict.unattributed),
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        velocity_quoter_health::{observe::FailReason, Charge, Verdict},
    };

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    #[test]
    fn a_named_quoter_is_dropped_from_the_next_round() {
        let verdict = Verdict {
            charges: vec![Charge {
                quoter: key(1),
                reason: FailReason::Cpi,
                proof: Attribution::Named,
            }],
            suspects: vec![],
            unattributed: None,
        };
        let plan = plan_retry(&verdict, &[]);
        assert_eq!(plan.drop, vec![key(1)]);
        assert_eq!(plan.reason, Some(FailReason::Cpi));
    }

    #[test]
    fn a_suspect_is_dropped_too_because_that_is_how_it_becomes_proof() {
        let verdict = Verdict {
            charges: vec![],
            suspects: vec![key(2)],
            unattributed: None,
        };
        assert_eq!(plan_retry(&verdict, &[]).drop, vec![key(2)]);
    }

    #[test]
    fn a_failure_naming_nobody_strips_nothing() {
        // Removing a quoter here would be a guess, and the market would lose
        // a source for a failure that was never a quoter's.
        let verdict = Verdict {
            charges: vec![],
            suspects: vec![],
            unattributed: Some(FailReason::Unknown),
        };
        assert!(plan_retry(&verdict, &[]).drop.is_empty());
    }

    #[test]
    fn a_round_that_names_only_what_is_already_gone_ends_the_loop() {
        let verdict = Verdict {
            charges: vec![Charge {
                quoter: key(3),
                reason: FailReason::Cpi,
                proof: Attribution::Named,
            }],
            suspects: vec![key(3)],
            unattributed: None,
        };
        assert!(plan_retry(&verdict, &[key(3)]).drop.is_empty());
    }
}

#[cfg(test)]
mod deploy_record_tests {
    use super::*;

    /// The watcher reads the deploy slot at a fixed offset rather than
    /// deserializing the whole record. A wrong offset would read garbage,
    /// change on every check, and hold every quoter in permanent probation,
    /// so the offset is pinned against the loader's own type.
    #[test]
    fn the_deploy_slot_sits_where_the_watcher_looks_for_it() {
        let state = solana_loader_v3_interface::state::UpgradeableLoaderState::ProgramData {
            slot: 0x0102_0304_0506_0708,
            upgrade_authority_address: Some(Pubkey::new_unique()),
        };
        let bytes = bincode::serialize(&state).expect("serialize");
        let read = u64::from_le_bytes(
            bytes[DEPLOY_SLOT_OFFSET..DEPLOY_SLOT_OFFSET + 8]
                .try_into()
                .expect("eight bytes"),
        );
        assert_eq!(read, 0x0102_0304_0506_0708);
    }

    /// A quoter's deploy record lives at a PDA of the upgradeable loader. A
    /// wrong loader address would find no account, and the watcher would
    /// silently never fire.
    #[test]
    fn the_loader_address_is_the_upgradeable_loader() {
        assert_eq!(
            UPGRADEABLE_LOADER,
            solana_sdk_ids::bpf_loader_upgradeable::ID
        );
    }
}

/// Sources one pass of the view may hold, matching the buffer's own cap.
///
/// The buffer rejects a push past this rather than truncating, so a pass that
/// would overflow it fails the whole market's quote. Every crossing DLOB
/// order takes a slot too, so quoters get what is left after them and the
/// vAMM.
const SOURCES_PER_PASS: usize = program::state::router_quote::MAX_QUOTED_SOURCES;

/// A market's whole book, read in as many passes as it takes.
pub struct MarketQuote {
    /// Every source across every pass, vAMM included exactly once.
    pub books: Vec<crate::quote_view::QuotedBook>,
    /// The entries behind those sources, for attributing their levels.
    pub entries: Vec<CarriedEntry>,
    /// Slot of the earliest pass. Passes run at different slots, so this is
    /// the age of the oldest thing in the merged book.
    pub slot: u64,
    /// Size the books were quoted at. A resting book is truncated by it; a
    /// PropAMM and the vAMM price against it, so their levels mean nothing
    /// without it.
    pub quoted_size: u64,
    pub units_consumed: u64,
    pub excluded: Vec<Pubkey>,
}

/// Quote a whole market, in as many passes as its quoters need.
///
/// One pass carries a fixed number of sources and a fixed number of accounts,
/// so a market with more quoters than that cannot be read at all in a single
/// call: the buffer refuses the push and the market goes dark rather than
/// degrading. Reading it in passes removes the ceiling, because the view is a
/// simulation and nothing about it has to be one transaction.
///
/// The vAMM rides exactly one pass. It shades against the books carried
/// alongside it, so a pass holding a subset would return a vAMM shaded
/// against a subset. The pass that carries it is the one holding the DLOB
/// makers, which are the rivals the fill will also put in front of it.
pub async fn quote_market<S: ChainSource>(
    source: &S,
    health: &Health,
    request: &QuoteRequest<'_>,
    entries: &[Pubkey],
) -> Result<MarketQuote> {
    // The DLOB makers and the vAMM take slots of their own on the pass that
    // carries them, so that pass holds fewer quoters, sometimes none. Later
    // passes still carry the rest, so the plan advances either way.
    //
    // More makers than a pass can hold would fail it outright, so the list is
    // cut to what fits. Cutting understates depth; overflowing publishes
    // nothing at all.
    let maker_room = SOURCES_PER_PASS - 1;
    let dlob_makers = &request.dlob_makers[..request.dlob_makers.len().min(maker_room)];
    if dlob_makers.len() < request.dlob_makers.len() {
        tracing::warn!(
            market = request.market_index,
            passed = request.dlob_makers.len(),
            carried = dlob_makers.len(),
            "more DLOB makers than one pass holds; the rest are not quoted"
        );
    }
    let first_pass_room = maker_room - dlob_makers.len();

    let (first, rest) = entries.split_at(first_pass_room.min(entries.len()));
    let mut passes: Vec<(&[Pubkey], bool, bool)> = vec![(first, true, true)];
    passes.extend(
        rest.chunks(SOURCES_PER_PASS)
            .map(|chunk| (chunk, false, false)),
    );

    let mut merged = MarketQuote {
        books: Vec::new(),
        entries: Vec::new(),
        slot: u64::MAX,
        quoted_size: request.size,
        units_consumed: 0,
        excluded: Vec::new(),
    };
    for (only, include_vamm, with_dlob) in passes {
        let pass = QuoteRequest {
            only: Some(only),
            include_vamm,
            dlob_makers: if with_dlob { dlob_makers } else { &[] },
            ..*request
        };
        let quoted = quote_with_health(source, health, &pass).await?;
        merged.slot = merged.slot.min(quoted.view.slot);
        merged.units_consumed += quoted.units_consumed;
        merged.books.extend(quoted.view.books);
        merged.entries.extend(quoted.entries);
        for key in quoted.excluded {
            if !merged.excluded.contains(&key) {
                merged.excluded.push(key);
            }
        }
    }
    if merged.slot == u64::MAX {
        merged.slot = 0;
    }
    Ok(merged)
}

#[cfg(test)]
mod pass_tests {
    use super::*;

    /// The pass plan a market of `entries` quoters and `dlob` makers produces.
    fn plan(entries: usize, dlob: usize) -> Vec<(usize, bool)> {
        let keys: Vec<Pubkey> = (0..entries).map(|_| Pubkey::new_unique()).collect();
        let maker_room = SOURCES_PER_PASS - 1;
        let carried_dlob = dlob.min(maker_room);
        let room = maker_room - carried_dlob;
        let (first, rest) = keys.split_at(room.min(keys.len()));
        let mut out = vec![(first.len(), true)];
        out.extend(rest.chunks(SOURCES_PER_PASS).map(|c| (c.len(), false)));
        out
    }

    #[test]
    fn a_small_market_is_read_in_one_pass() {
        assert_eq!(plan(3, 0), vec![(3, true)]);
    }

    #[test]
    fn the_vamm_rides_exactly_one_pass() {
        // Two vAMM ladders in a merged book would be two different answers to
        // the same question, because each shades against its own pass.
        let vamm_passes = plan(40, 0).iter().filter(|(_, vamm)| *vamm).count();
        assert_eq!(vamm_passes, 1);
    }

    #[test]
    fn no_pass_can_overflow_the_buffer() {
        // The buffer errors rather than truncating, so a pass that would
        // exceed it takes the whole market's quote down.
        for (entries, dlob) in [(40, 0), (100, 0), (17, 0), (40, 10), (5, 15)] {
            for (i, (count, vamm)) in plan(entries, dlob).iter().enumerate() {
                let carried_dlob = dlob.min(SOURCES_PER_PASS - 1);
                let sources = count + usize::from(*vamm) + if i == 0 { carried_dlob } else { 0 };
                assert!(
                    sources <= SOURCES_PER_PASS,
                    "pass {i} of ({entries}, {dlob}) holds {sources} sources"
                );
            }
        }
    }

    #[test]
    fn every_quoter_lands_on_exactly_one_pass() {
        for entries in [0, 1, 15, 16, 17, 100] {
            let total: usize = plan(entries, 0).iter().map(|(count, _)| count).sum();
            assert_eq!(total, entries);
        }
    }

    #[test]
    fn a_market_crowded_with_dlob_makers_still_quotes_every_quoter() {
        // The makers and the vAMM can fill the first pass on their own. The
        // quoters then ride later passes rather than being squeezed into a
        // pass that would overflow and take the whole market down.
        let plan = plan(4, 20);
        assert_eq!(plan[0].0, 0, "no room left on the vAMM pass");
        assert_eq!(plan.iter().map(|(c, _)| c).sum::<usize>(), 4);
    }
}
