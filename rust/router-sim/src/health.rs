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
        build_quote_router_ix, pass_account_cost, read_zero_copy, simulate_quote_view_with_cost,
        CarriedEntry, QuoteRouterParams, QuoteSimFailure, QuoteView, PASS_ACCOUNT_BUDGET,
        PASS_FIXED_ACCOUNTS,
    },
    anyhow::Result,
    program::state::prop_amm::QuoterV0,
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
            // Health probes and the published books price protected flow:
            // the full view, as an attested taker or a crank would see it.
            taker_served_window: true,
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
    /// A pass filled its row region, so some book's orders are described
    /// only in part. The ladders are whole either way.
    pub rows_truncated: bool,
}

/// One simulated call: the entries it carries, and whether the DLOB makers
/// and the vAMM ride with them.
struct Pass {
    entries: Vec<Pubkey>,
    include_vamm: bool,
    with_dlob: bool,
}

/// How a market's quoters divide into passes that fit.
struct Plan {
    passes: Vec<Pass>,
    /// DLOB makers the first pass could take. Cutting understates depth;
    /// overflowing publishes nothing at all.
    carried_dlob: usize,
    /// Entries no pass can carry even alone, because their own CPI surface
    /// outgrows a transaction. Reported rather than dropped silently.
    unquotable: Vec<Pubkey>,
}

/// Split a market's quoters into passes, under both ceilings that bind.
///
/// They bind differently, which is why counting one is not enough. The buffer
/// holds [`SOURCES_PER_PASS`] sources and refuses a push past it, so a pass
/// that overruns *it* fails inside velocity and takes that pass's book down.
/// The transaction holds [`PASS_ACCOUNT_BUDGET`] keys, and a pass that
/// overruns *that* is rejected by the runtime before velocity runs at all.
///
/// Sources used to be the only ceiling this planner counted. Sixteen of them
/// is far more than a transaction can address — a quoter with its own user
/// pair and CPI accounts costs four keys — so a market with a handful of
/// quoters planned as one pass and then could not be sent.
fn plan_passes(entries: &[(Pubkey, QuoterV0)], dlob_makers: usize) -> Plan {
    // The vAMM takes a source slot on the pass that carries it and no
    // accounts of its own: the perp market it reads is already there.
    let carried_dlob = dlob_makers
        .min(SOURCES_PER_PASS - 1)
        .min((PASS_ACCOUNT_BUDGET.saturating_sub(PASS_FIXED_ACCOUNTS)) / 2);

    let mut passes: Vec<Pass> = Vec::new();
    let mut unquotable: Vec<Pubkey> = Vec::new();
    let mut open: Vec<(Pubkey, QuoterV0)> = Vec::new();
    let mut first = true;

    // A pass in progress, plus one more entry: does it still fit?
    let fits = |held: &[(Pubkey, QuoterV0)], first: bool| {
        let dlob = if first { carried_dlob } else { 0 };
        let sources = held.len() + dlob + usize::from(first);
        let data: Vec<QuoterV0> = held.iter().map(|(_, entry)| *entry).collect();
        sources <= SOURCES_PER_PASS && pass_account_cost(&data, dlob) <= PASS_ACCOUNT_BUDGET
    };

    for (key, entry) in entries {
        open.push((*key, *entry));
        if fits(&open, first) {
            continue;
        }
        open.pop();

        // It did not fit, so close the pass in progress and try it on a
        // fresh one. The first pass closes even holding no quoters at all:
        // the makers and the vAMM can fill it on their own, and it is still
        // the pass that carries them.
        if !open.is_empty() || first {
            passes.push(Pass {
                entries: open.iter().map(|(key, _)| *key).collect(),
                include_vamm: first,
                with_dlob: first,
            });
            first = false;
            open.clear();
        }

        open.push((*key, *entry));
        if !fits(&open, first) {
            // Alone on an empty pass and still too wide: nothing can carry
            // it, so the market publishes without it rather than not at all.
            open.pop();
            unquotable.push(*key);
        }
    }
    if !open.is_empty() || passes.is_empty() {
        passes.push(Pass {
            entries: open.iter().map(|(key, _)| *key).collect(),
            include_vamm: first,
            with_dlob: first,
        });
    }

    Plan {
        passes,
        carried_dlob,
        unquotable,
    }
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
    // The planner sizes a pass by what it costs to *send*, so it needs each
    // entry's registered CPI surface. The accounts are already resident —
    // this is the read the builder does again a moment later.
    let carried: Vec<(Pubkey, QuoterV0)> = entries
        .iter()
        .copied()
        .zip(source.get_multiple_accounts(entries).await?)
        .filter_map(|(key, account)| {
            let entry = read_zero_copy::<QuoterV0>(&account?.data).ok()?;
            Some((key, entry))
        })
        .collect();
    let plan = plan_passes(&carried, request.dlob_makers.len());

    if plan.carried_dlob < request.dlob_makers.len() {
        tracing::warn!(
            market = request.market_index,
            passed = request.dlob_makers.len(),
            carried = plan.carried_dlob,
            "more DLOB makers than one pass holds; the rest are not quoted"
        );
    }
    for key in &plan.unquotable {
        tracing::warn!(
            market = request.market_index,
            quoter = %key,
            "quoter needs more accounts than one transaction holds; not quoted"
        );
    }
    let dlob_makers = &request.dlob_makers[..plan.carried_dlob];

    let mut merged = MarketQuote {
        books: Vec::new(),
        entries: Vec::new(),
        slot: u64::MAX,
        quoted_size: request.size,
        units_consumed: 0,
        excluded: Vec::new(),
        rows_truncated: false,
    };
    for planned in &plan.passes {
        let pass = QuoteRequest {
            only: Some(&planned.entries),
            include_vamm: planned.include_vamm,
            dlob_makers: if planned.with_dlob { dlob_makers } else { &[] },
            ..*request
        };
        let quoted = quote_with_health(source, health, &pass).await?;
        merged.slot = merged.slot.min(quoted.view.slot);
        merged.units_consumed += quoted.units_consumed;
        merged.books.extend(quoted.view.books);
        merged.rows_truncated |= quoted.view.rows_truncated;
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
    use {
        super::*,
        program::state::prop_amm::{AmmAccountMeta, QuoterType, MAX_QUOTER_ACCOUNTS},
    };

    fn sysvar() -> Pubkey {
        Pubkey::new_from_array([1; 32])
    }

    fn state() -> Pubkey {
        Pubkey::new_from_array([2; 32])
    }

    /// A quoter shaped like the midpoint: an instance of a shared program,
    /// filling for its own user. Four keys a pass has to find room for — the
    /// entry, the instance, and the user's two accounts — plus the program
    /// and the sysvar every instance shares.
    fn custom(program: Pubkey) -> (Pubkey, QuoterV0) {
        let mut entry: QuoterV0 = bytemuck::Zeroable::zeroed();
        let instance = Pubkey::new_unique();
        entry.program_id = program;
        entry.response_account = instance;
        entry.user = Pubkey::new_unique();
        entry.quoter_type = QuoterType::Custom;
        entry.quote_accounts_count = 3;
        for (slot, pubkey) in [instance, sysvar(), state()].into_iter().enumerate() {
            entry.quote_accounts[slot] = AmmAccountMeta {
                pubkey,
                is_writable: slot == 0,
                padding: [0; 7],
            };
        }
        (Pubkey::new_unique(), entry)
    }

    /// One quoter whose own CPI surface is wider than a whole transaction.
    fn oversized() -> (Pubkey, QuoterV0) {
        let (key, mut entry) = custom(Pubkey::new_unique());
        // Every slot the registry gives one entry, which is already more
        // keys than a transaction has room for.
        entry.quote_accounts_count = MAX_QUOTER_ACCOUNTS as u8;
        for slot in 0..entry.quote_accounts_count as usize {
            entry.quote_accounts[slot] = AmmAccountMeta {
                pubkey: Pubkey::new_unique(),
                is_writable: false,
                padding: [0; 7],
            };
        }
        (key, entry)
    }

    /// A market of `count` midpoint-shaped quoters on one program.
    fn market(count: usize) -> Vec<(Pubkey, QuoterV0)> {
        let program = Pubkey::new_unique();
        (0..count).map(|_| custom(program)).collect()
    }

    /// What every pass costs, as the transaction counts it.
    fn accounts_per_pass(entries: &[(Pubkey, QuoterV0)], plan: &Plan) -> Vec<usize> {
        plan.passes
            .iter()
            .map(|pass| {
                let data: Vec<QuoterV0> = pass
                    .entries
                    .iter()
                    .map(|key| {
                        entries
                            .iter()
                            .find(|(entry_key, _)| entry_key == key)
                            .expect("a planned entry is one of the market's")
                            .1
                    })
                    .collect();
                pass_account_cost(&data, if pass.with_dlob { plan.carried_dlob } else { 0 })
            })
            .collect()
    }

    #[test]
    fn a_small_market_is_read_in_one_pass() {
        let plan = plan_passes(&market(3), 0);
        assert_eq!(plan.passes.len(), 1);
        assert_eq!(plan.passes[0].entries.len(), 3);
    }

    #[test]
    fn the_vamm_rides_exactly_one_pass() {
        // Two vAMM ladders in a merged book would be two different answers to
        // the same question, because each shades against its own pass.
        let plan = plan_passes(&market(40), 0);
        assert_eq!(
            plan.passes.iter().filter(|pass| pass.include_vamm).count(),
            1
        );
    }

    /// The ceiling this planner used to miss. Sixteen sources is far more
    /// than a transaction can address — a quoter with its own user pair and
    /// CPI accounts costs four keys — so counting sources alone planned
    /// passes the runtime rejected before velocity ran.
    #[test]
    fn no_pass_outgrows_the_transaction_that_has_to_carry_it() {
        for (count, dlob) in [(1, 0), (4, 0), (8, 0), (40, 0), (8, 4), (4, 20), (100, 3)] {
            let entries = market(count);
            let plan = plan_passes(&entries, dlob);
            for (index, accounts) in accounts_per_pass(&entries, &plan).iter().enumerate() {
                assert!(
                    *accounts <= PASS_ACCOUNT_BUDGET,
                    "pass {index} of ({count}, {dlob}) needs {accounts} keys"
                );
            }
        }
    }

    #[test]
    fn no_pass_can_overflow_the_buffer() {
        // The buffer errors rather than truncating, so a pass that would
        // exceed it takes that pass's book down.
        for (count, dlob) in [(40, 0), (100, 0), (17, 0), (40, 10), (5, 15)] {
            let plan = plan_passes(&market(count), dlob);
            for (index, pass) in plan.passes.iter().enumerate() {
                let sources = pass.entries.len()
                    + usize::from(pass.include_vamm)
                    + if pass.with_dlob { plan.carried_dlob } else { 0 };
                assert!(
                    sources <= SOURCES_PER_PASS,
                    "pass {index} of ({count}, {dlob}) holds {sources} sources"
                );
            }
        }
    }

    #[test]
    fn every_quoter_lands_on_exactly_one_pass() {
        for count in [0, 1, 15, 16, 17, 100] {
            let entries = market(count);
            let plan = plan_passes(&entries, 0);
            let mut planned: Vec<Pubkey> = plan
                .passes
                .iter()
                .flat_map(|pass| pass.entries.iter().copied())
                .collect();
            planned.sort();
            let before = planned.len();
            planned.dedup();
            assert_eq!(planned.len(), before, "a quoter rode two passes");
            assert_eq!(planned.len(), count);
            assert!(plan.unquotable.is_empty());
        }
    }

    /// A quoter no transaction can carry is named, not silently skipped: the
    /// market publishes without it, and an operator can see why.
    #[test]
    fn a_quoter_wider_than_a_transaction_is_reported() {
        let mut entries = market(2);
        let (wide, entry) = oversized();
        entries.insert(1, (wide, entry));
        let plan = plan_passes(&entries, 0);
        assert_eq!(plan.unquotable, vec![wide]);
        let planned: Vec<Pubkey> = plan
            .passes
            .iter()
            .flat_map(|pass| pass.entries.iter().copied())
            .collect();
        assert_eq!(planned.len(), 2, "the other two still quote");
        assert!(!planned.contains(&wide));
    }

    #[test]
    fn a_market_crowded_with_dlob_makers_still_quotes_every_quoter() {
        // The makers can fill the first pass on their own. The quoters then
        // ride later passes rather than being squeezed into one that cannot
        // be sent.
        let entries = market(4);
        let plan = plan_passes(&entries, 20);
        assert!(plan.carried_dlob > 0 && plan.carried_dlob < 20);
        let planned: usize = plan.passes.iter().map(|pass| pass.entries.len()).sum();
        assert_eq!(planned, 4);
        for accounts in accounts_per_pass(&entries, &plan) {
            assert!(accounts <= PASS_ACCOUNT_BUDGET);
        }
    }
}
