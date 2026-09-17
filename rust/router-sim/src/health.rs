//! Quote a market without letting one quoter silence it.
//!
//! `quote_router` calls every registry entry the instruction carries. One entry
//! that reverts fails the whole simulation. A single bad quoter therefore stops
//! a market's book from publishing and turns a route request into an error. The
//! market's other sources were fine, and nothing reached them.
//!
//! When a simulation fails, this module reads the logs, works out which entry
//! caused the failure, drops that entry, and simulates again. The caller gets
//! the market without the bad quoter instead of nothing.
//!
//! The retry is also the measurement. A log line that names a quoter is strong
//! evidence. A simulation that passes once one entry is removed is proof, and
//! it costs nothing extra, because the router wants the retry anyway. This
//! module claims proof only when it removed exactly one entry. Dropping two at
//! once and succeeding shows that at least one was at fault, not which one.

use {
    crate::quote_view::{
        build_quote_router_ix, pass_account_cost, simulate_quote_view_with_cost, CarriedEntry,
        QuoteRouterParams, QuoteSimFailure, QuoteView, PASS_ACCOUNT_BUDGET, PASS_FIXED_ACCOUNTS,
    },
    anyhow::Result,
    program::state::prop_amm::QuoterSlotV0,
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
/// A redeploy leaves the score describing code that no longer runs. The health
/// layer therefore drops the counters and puts the quoter on probation instead
/// of giving it full flow. A broken rollout then degrades and recovers with no
/// operator. A program that serves many registry entries moves all of them,
/// because the upgrade changed the code behind every one.
///
/// A program that is not upgradeable has no deploy record, and this function
/// skips it. Its code cannot change, so its score cannot go stale.
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
    /// Carry only these entries. `None` carries every live entry, which fits
    /// only while a market has few enough of them.
    pub only: Option<&'a [Pubkey]>,
    /// Quote the vAMM into this pass.
    pub include_vamm: bool,
}

impl<'a> QuoteRequest<'a> {
    /// The fields that quote a whole market in one pass.
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
            // Health probes and the published books price protected flow.
            // They show the full view, as an attested taker or a crank sees it.
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

        // A throttled quoter rides a fraction of routes. This decision runs on
        // the entries this market holds, so one market's traffic does not spend
        // another market's sampling budget.
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
                // A success after exactly one entry was removed proves that
                // entry caused the failure. A success after several were
                // removed proves only that one of them did, so none is charged.
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
                // The view reports whether margin verification cut a book below
                // what its source quoted. It reports that a cut happened, not
                // how deep. This therefore counts the share of quotes a quoter
                // had cut, not the share of base lost. Both measure whether the
                // quoter offers depth the account behind it cannot carry.
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
                    // Not a simulation failure, so no quoter caused it.
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
                    // guess. Return the failure instead of stripping the
                    // market down.
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
/// The plan drops charges and suspects alike. The program names a charge. The
/// CPI brackets imply a suspect. Removing either is how a suspect becomes
/// proof. The plan does not list an entry that is already excluded, so a round
/// that names only what is already gone drops nothing and ends the loop.
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
        // a source for a failure that no quoter caused.
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

    /// The watcher reads the deploy slot at a fixed offset instead of
    /// deserializing the whole record. A wrong offset reads garbage that
    /// changes on every check, and it holds every quoter in permanent
    /// probation. This test pins the offset against the loader's own type.
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
    /// wrong loader address finds no account, and the watcher never fires.
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
    /// Size the books were quoted at. It truncates a resting book. A PropAMM
    /// and the vAMM price against it, so their levels mean nothing without it.
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
    /// DLOB makers the first pass could take. A cut understates depth. An
    /// overflow publishes nothing.
    carried_dlob: usize,
    /// Entries no pass can carry, even alone, because their own CPI surface
    /// outgrows a transaction. The planner reports them instead of dropping
    /// them.
    unquotable: Vec<Pubkey>,
}

/// Split a market's quoters into passes, under both ceilings that bind.
///
/// The two ceilings bind differently, so counting one is not enough. The buffer
/// holds [`SOURCES_PER_PASS`] sources and refuses a push past that count. A
/// pass that overruns the buffer fails inside velocity and takes that pass's
/// book down. The transaction holds [`PASS_ACCOUNT_BUDGET`] keys. The runtime
/// rejects a pass that overruns the key budget before velocity runs at all.
///
/// The source count alone does not bound the key count. A quoter with its own
/// user pair and CPI accounts costs four keys, so a market of a few quoters can
/// sit under [`SOURCES_PER_PASS`] and still be too wide to send.
fn plan_passes(slots: &[QuoterSlotV0], dlob_makers: usize) -> Plan {
    // The vAMM takes a source slot on the pass that carries it. It needs no
    // accounts of its own, because the perp market it reads is already there.
    let carried_dlob = dlob_makers
        .min(SOURCES_PER_PASS - 1)
        .min((PASS_ACCOUNT_BUDGET.saturating_sub(PASS_FIXED_ACCOUNTS)) / 2);

    let mut passes: Vec<Pass> = Vec::new();
    let mut unquotable: Vec<Pubkey> = Vec::new();
    let mut open: Vec<QuoterSlotV0> = Vec::new();
    let mut first = true;

    // Whether a pass in progress still fits after one more quoter.
    let fits = |held: &[QuoterSlotV0], first: bool| {
        let dlob = if first { carried_dlob } else { 0 };
        let sources = held.len() + dlob + usize::from(first);
        sources <= SOURCES_PER_PASS && pass_account_cost(held, dlob) <= PASS_ACCOUNT_BUDGET
    };

    for slot in slots {
        open.push(*slot);
        if fits(&open, first) {
            continue;
        }
        open.pop();

        // The quoter did not fit, so close the pass in progress and try the
        // quoter on a fresh pass. The first pass closes even when it holds no
        // quoters. The makers and the vAMM can fill it on their own, and it is
        // still the pass that carries them.
        if !open.is_empty() || first {
            passes.push(Pass {
                entries: open.iter().map(|slot| slot.entry).collect(),
                include_vamm: first,
                with_dlob: first,
            });
            first = false;
            open.clear();
        }

        open.push(*slot);
        if !fits(&open, first) {
            // The quoter is alone on an empty pass and still too wide. No pass
            // can carry it, so the market publishes without it instead of not
            // at all.
            open.pop();
            unquotable.push(slot.entry);
        }
    }
    if !open.is_empty() || passes.is_empty() {
        passes.push(Pass {
            entries: open.iter().map(|slot| slot.entry).collect(),
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
/// One pass carries a fixed number of sources and a fixed number of accounts.
/// A market with more quoters than that cannot be read in a single call. The
/// buffer refuses the push, and the market then publishes nothing instead of a
/// smaller book. Several passes remove the ceiling, because the view is a
/// simulation and it does not have to be one transaction.
///
/// The vAMM rides exactly one pass. It shades against the books carried with
/// it, so a pass holding a subset would return a vAMM shaded against a subset.
/// The pass that carries the vAMM is the one that holds the DLOB makers,
/// because the fill puts those same makers in front of it.
pub async fn quote_market<S: ChainSource>(
    source: &S,
    health: &Health,
    request: &QuoteRequest<'_>,
    entries: &[Pubkey],
) -> Result<MarketQuote> {
    // The planner sizes a pass by what it costs to send, so it needs each
    // quoter's registered CPI surface. That comes off the market's slab, the
    // same copy the builder reads next. The slab keeps slot order, which is the
    // order the on-chain walk consults.
    let slots = crate::quoter_slab_slots(source, &request.velocity, request.market_index).await?;
    let carried: Vec<QuoterSlotV0> = slots
        .into_iter()
        .filter(|slot| slot.quotes() && entries.contains(&slot.entry))
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

    /// A quoter shaped like the midpoint. It is an instance of a shared
    /// program, and it fills for its own user. A pass must find room for three
    /// unshared keys: the instance and the user's two accounts. It also carries
    /// the program and the sysvar that every instance shares.
    fn custom(program: Pubkey) -> QuoterSlotV0 {
        let mut slot: QuoterSlotV0 = bytemuck::Zeroable::zeroed();
        let instance = Pubkey::new_unique();
        slot.entry = Pubkey::new_unique();
        slot.config.is_active = true;
        slot.config.program_id = program;
        slot.config.response_account = instance;
        slot.config.user = Pubkey::new_unique();
        slot.config.quoter_type = QuoterType::Custom;
        slot.config.accounts_count = 3;
        slot.config.quote_accounts_count = 3;
        for (index, pubkey) in [instance, sysvar(), state()].into_iter().enumerate() {
            slot.config.accounts[index] = AmmAccountMeta {
                pubkey,
                is_writable: index == 0,
                padding: [0; 7],
            };
            slot.config.quote_account_indexes[index] = index as u8;
        }
        slot
    }

    /// One quoter with the widest registered surface the program allows,
    /// every account unshared.
    fn widest() -> QuoterSlotV0 {
        let mut slot = custom(Pubkey::new_unique());
        slot.config.accounts_count = MAX_QUOTER_ACCOUNTS as u8;
        for index in 0..slot.config.accounts_count as usize {
            slot.config.accounts[index] = AmmAccountMeta {
                pubkey: Pubkey::new_unique(),
                is_writable: false,
                padding: [0; 7],
            };
        }
        slot
    }

    /// A market of `count` midpoint-shaped quoters on one program.
    fn market(count: usize) -> Vec<QuoterSlotV0> {
        let program = Pubkey::new_unique();
        (0..count).map(|_| custom(program)).collect()
    }

    /// What every pass costs, as the transaction counts it.
    fn accounts_per_pass(slots: &[QuoterSlotV0], plan: &Plan) -> Vec<usize> {
        plan.passes
            .iter()
            .map(|pass| {
                let data: Vec<QuoterSlotV0> = pass
                    .entries
                    .iter()
                    .map(|key| {
                        *slots
                            .iter()
                            .find(|slot| slot.entry == *key)
                            .expect("a planned entry is one of the market's")
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

    /// The transaction key budget binds before the source count does. A quoter
    /// with its own user pair and CPI accounts costs four keys, so a plan that
    /// counts sources alone produces passes the runtime rejects before velocity
    /// runs.
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

    /// The registered list is capped at `MAX_QUOTER_ACCOUNTS`, and a quoter at
    /// that cap fits one transaction on its own. The planner therefore places
    /// the widest allowed surface on a pass and never reports it unquotable.
    /// The `unquotable` report guards a budget change that breaks this.
    #[test]
    fn a_quoter_with_the_widest_allowed_surface_still_quotes() {
        let mut entries = market(2);
        let wide_slot = widest();
        let wide = wide_slot.entry;
        entries.insert(1, wide_slot);
        let plan = plan_passes(&entries, 0);
        assert!(plan.unquotable.is_empty());
        let planned: Vec<Pubkey> = plan
            .passes
            .iter()
            .flat_map(|pass| pass.entries.iter().copied())
            .collect();
        assert_eq!(planned.len(), 3, "every quoter quotes");
        assert!(planned.contains(&wide));
        for accounts in accounts_per_pass(&entries, &plan) {
            assert!(accounts <= PASS_ACCOUNT_BUDGET);
        }
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
