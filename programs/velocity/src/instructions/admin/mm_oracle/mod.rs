//! The market-maker oracle price, written through native dispatch.
//!
//! Two instructions write it: one market per instruction, and one batch of up
//! to 64 markets. Both run before Anchor, so they re-establish the ownership
//! and discriminator guarantees Anchor would give, and they bounds-check every
//! index before use.
//!
//! [`apply_mm_oracle_update`] is the one copy of the per-market gating, so the
//! two instructions cannot drift apart. The wrappers differ only in how they
//! report a skip.

use super::{slot_duration::read_native_state_slot_clock, *};

#[cfg(test)]
mod tests;

/// Byte offset of `State::feature_bit_flags` from the start of the account data
/// (including the 8-byte Anchor discriminator). The native handlers read it by
/// raw index rather than deserializing all of `State`. Guarded by
/// `state/traits/tests.rs::native_instruction_offsets`.
const STATE_FEATURE_BIT_FLAGS_OFFSET: usize = 1374;

/// Byte offset of `State::hot_mm_oracle_crank` (32 bytes) from the start of the
/// account data. Same guard as above. Only read outside `anchor-test`, which
/// compiles the signer checks out.
#[cfg_attr(feature = "anchor-test", allow(dead_code))]
const STATE_HOT_MM_ORACLE_CRANK_OFFSET: usize = 360;

pub fn handle_update_mm_oracle_native(accounts: &[AccountInfo], data: &[u8]) -> Result<()> {
    // Slot comes from the Clock sysvar syscall: no clock account, nothing for
    // a caller to forge, one account fewer per transaction.
    update_mm_oracle(accounts, data, Clock::get()?.slot)
}

/// Body of `handle_update_mm_oracle_native` (native dispatch opcode 0), split
/// from the syscall so tests can drive the slot directly.
///
/// Pre-Anchor native dispatch: re-establishes the ownership + discriminator
/// guarantees Anchor would provide (see `crate::auth::require_native_account`)
/// before trusting any byte. Accounts:
///   `[0]` perp_market (mut), `[1]` signer, `[2]` state.
/// Payload: `i64 price | u64 sequence_id | u64 source_slot` (all LE, 24 bytes).
/// State byte offsets are `STATE_*_OFFSET` above
/// (guarded by `state/traits/tests.rs::native_instruction_offsets`).
///
/// Every index is bounds-checked before use: this runs before Anchor, so a
/// malformed instruction arrives verbatim, and a short account list or payload
/// used to panic, which aborts the transaction with no identifiable error and
/// burns the whole compute budget getting there.
///
/// After authentication the per-market gating is `apply_mm_oracle_update`, the
/// same core the batch handler (opcode 2) runs. The differences are all in this
/// prologue: a non-positive price is a hard `Err` here (the batch skips the
/// entry, since a hard error there would destroy every other market's write),
/// there is no market index in the payload to cross-check, and skips are logged
/// per reason where the batch logs one reject mask.
fn update_mm_oracle(accounts: &[AccountInfo], data: &[u8], current_slot: u64) -> Result<()> {
    require!(accounts.len() >= 3, ErrorCode::InvalidNativeInstructionData);
    require!(data.len() >= 24, ErrorCode::InvalidNativeInstructionData);

    crate::auth::require_native_account(
        &accounts[2],
        State::DISCRIMINATOR,
        ErrorCode::InvalidNativeStateAccount,
    )?;
    crate::auth::require_native_account(
        &accounts[0],
        PerpMarket::DISCRIMINATOR,
        ErrorCode::InvalidNativePerpMarketAccount,
    )?;

    require_mm_oracle_crank(&accounts[2], &accounts[1])?;

    // Non-positive prices are a hard error. Rejecting only exact zero left a
    // hole once the step cap clamped instead of skipping: a negative target was
    // clamped against the stored price and *written* (e.g. -1 against 1,000,000
    // landed as 990,000, consuming the sequence id), and repeated negatives
    // could walk the price to zero, resetting the bootstrap path and with it
    // the step cap.
    let incoming_price = i64::from_le_bytes(data[0..8].try_into().unwrap());
    if incoming_price <= 0 {
        msg!("MM oracle price is non-positive, not updating");
        return Err(ErrorCode::DefaultError.into());
    }
    let incoming_sequence_id = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let source_slot = u64::from_le_bytes(data[16..24].try_into().unwrap());

    let outcome = apply_mm_oracle_update(
        &accounts[0],
        MmOraclePayload {
            expected_market_index: None,
            price: incoming_price,
            sequence_id: incoming_sequence_id,
            source_slot,
        },
        current_slot,
        read_native_state_slot_clock(&accounts[2])?,
    )?;

    log_mm_oracle_outcome(outcome, incoming_price, current_slot);

    Ok(())
}

/// Authenticates the caller of a native MM oracle instruction.
///
/// The kill switch is read first, so an operator can stop the crank without an
/// upgrade and the failure names its own error code. The signer must then be
/// the configured hot key. An `anchor-test` build compiles the signer check out,
/// because the test harness signs as itself.
///
/// The caller MUST have already passed `state_account` through
/// `crate::auth::require_native_account`. This function reads raw bytes at fixed
/// offsets and would otherwise trust caller-chosen data.
fn require_mm_oracle_crank(
    state_account: &AccountInfo,
    signer_account: &AccountInfo,
) -> Result<()> {
    let state = state_account.try_borrow_data()?;

    let feature_bit_flags = *state
        .get(STATE_FEATURE_BIT_FLAGS_OFFSET)
        .ok_or(ErrorCode::InvalidNativeStateAccount)?;
    require!(
        feature_bit_flags & (FeatureBitFlags::MmOracleUpdate as u8) > 0,
        ErrorCode::MmOracleUpdateDisabled
    );

    #[cfg(not(feature = "anchor-test"))]
    {
        let hot_key_bytes: [u8; 32] = state
            .get(STATE_HOT_MM_ORACLE_CRANK_OFFSET..STATE_HOT_MM_ORACLE_CRANK_OFFSET + 32)
            .ok_or(ErrorCode::InvalidNativeStateAccount)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidNativeStateAccount)?;
        let hot_key = anchor_lang::prelude::Pubkey::new_from_array(hot_key_bytes);
        require!(
            signer_account.is_signer && *signer_account.key == hot_key,
            ErrorCode::Unauthorized
        );
    }

    #[cfg(feature = "anchor-test")]
    let _ = signer_account;

    Ok(())
}

/// Logs the result of the single-market handler with its values. The batch logs
/// one mask per reason class instead, because formatted logging costs about
/// 700 compute units per call.
fn log_mm_oracle_outcome(outcome: MmOracleUpdateOutcome, incoming_price: i64, current_slot: u64) {
    match outcome {
        MmOracleUpdateOutcome::Written { price } => {
            if price != incoming_price {
                msg!(
                    "mm oracle step clamped: incoming={} written={}",
                    incoming_price,
                    price
                );
            }
        }
        // Stale sequence id is the crank's ordinary redundant-send case and
        // stays silent, matching the pre-batch handler. The rest are logged
        // with their values; the batch logs a mask instead.
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::StaleSequenceId) => {}
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::SlotNotAdvanced { stored_slot }) => {
            msg!(
                "mm oracle reject: stale slot {} <= {}",
                current_slot,
                stored_slot
            );
        }
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::RecrankGapTooSmall { gap, min_gap }) => {
            msg!(
                "mm oracle reject: re-crank gap {} slots < {} slots",
                gap,
                min_gap
            );
        }
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::SourceSlotOutOfRange {
            source_slot,
        }) => {
            msg!(
                "mm oracle reject: source slot {} out of range at slot {}",
                source_slot,
                current_slot
            );
        }
        // Unreachable behind the hard error in the caller; kept exhaustive so a
        // new skip reason cannot be silently swallowed here.
        MmOracleUpdateOutcome::Skipped(MmOracleSkipReason::NonPositivePrice) => {
            msg!("MM oracle price is non-positive, not updating");
        }
    }
}

/// Maximum markets one batch may carry. Bounds the reject-mask width (`u64`) and
/// the worst-case CU of a single instruction. Not a practical restriction: the
/// transaction packet size and the runtime's 64 account-lock ceiling both bind
/// well before this does.
const MM_ORACLE_BATCH_MAX_MARKETS: usize = 64;

// `rejected_mask` is a u64 indexed by entry position, so the batch can never
// carry more entries than the mask has bits.
static_assertions::const_assert!(MM_ORACLE_BATCH_MAX_MARKETS <= u64::BITS as usize);

/// Fixed (non-market) accounts at the head of the batch account list.
const MM_ORACLE_BATCH_FIXED_ACCOUNTS: usize = 2;

/// Bytes per market entry in the batch payload: `u16` market index + `i64` price
/// + `u64` sequence id + `u64` source slot.
const MM_ORACLE_BATCH_ENTRY_LEN: usize = 26;

/// Writes the MM oracle price for many perp markets in one native instruction
/// (dispatch opcode 2).
///
/// Semantically identical to `handle_update_mm_oracle_native` applied once per
/// market, but the authentication prologue (state validation, kill switch, hot
/// key compare) is paid once for the whole batch instead of
/// once per market.
///
/// `bun run bench:native-cu` measures the result as exactly linear:
/// 1627 CU at one market, 2145 at two, 3181 at four, i.e. a ~1109 CU fixed
/// prologue plus ~518 CU per market. Four markets cost 3181 CU here against
/// 6320 CU as four separate instructions. Compute is the smaller half of the
/// saving: the per-signature transaction fee is flat and independent of how
/// much the instruction does, so collapsing N transactions into one is what
/// dominates for a caller cranking on a fixed slot interval.
///
/// # Accounts
///
/// - `[0]` signer, must equal `State::hot_mm_oracle_crank`
/// - `[1]` state, owner + discriminator checked
/// - `[2..2+n]` perp markets, writable, owner + discriminator checked, order
///   matches the payload
///
/// Accounts beyond `2 + n` are ignored. The slot comes from the Clock sysvar
/// syscall, so no clock account is passed and none can be forged.
///
/// # Payload (after the 5-byte native prefix)
///
/// ```text
/// byte 0        u8   n           number of market entries, 1..=64
/// bytes 1..     n x  { u16 market_index_le (2B), i64 price_le (8B),
///                      u64 sequence_id_le (8B), u64 source_slot_le (8B) }
/// ```
///
/// `source_slot` is the slot the crank observed the price at. It is not
/// stored; it only bounds how late a signed update may land (see
/// `MM_ORACLE_MAX_SOURCE_AGE`), since `mm_oracle_slot` is stamped with
/// the landing slot and would otherwise make an old observation read as fresh.
///
/// Entry `i` applies to account `2 + i`, and the entry's `market_index` must
/// equal that market's own `market_index`. The redundancy is deliberate: without
/// it the entry-to-market binding would be purely positional, so a single
/// off-by-one in a caller's account list would silently write one market's price
/// onto another and the transaction would still succeed. The step cap catches
/// that for a market with an established price, but a market still bootstrapping
/// from zero would accept the wrong price outright and then be wedged, because
/// every subsequent legitimate update fails the 1% step cap against it
/// (recoverable only via `zero_mm_oracle_fields`). Two bytes and one compare buy
/// a hard error instead.
///
/// # Failure model
///
/// The split between "abort the batch" and "skip this market" is deliberate.
///
/// **Hard errors (whole transaction fails).** Every one of these is a caller
/// bug, and the caller is the hot key, i.e. our own bot. Failing loudly is
/// correct: silently skipping a market the operator believes is being cranked
/// would reintroduce exactly the "landed but wrote nothing" blindness this
/// instruction is meant to reduce.
/// - malformed payload framing, `n == 0`, `n > MM_ORACLE_BATCH_MAX_MARKETS`
/// - too few accounts for the declared `n`
/// - state account not owned by this program / wrong discriminator
/// - kill switch off (`FeatureBitFlags::MmOracleUpdate` clear)
/// - signer is not the configured hot key
/// - any market account fails owner + discriminator, or is not writable
/// - any market's own `market_index` disagrees with its payload entry
///
/// **Soft skips (that market is left untouched, the batch continues).** These
/// are expected runtime conditions for any caller cranking near the program's
/// minimum slot gap, not errors. One
/// rate-limited market must never destroy the writes for the others.
/// - non-positive price
/// - sequence id not strictly greater than the stored one
/// - current slot not strictly greater than the stored slot
/// - slot gap below `MM_ORACLE_MIN_WRITE_GAP`
/// - source slot more than `MM_ORACLE_MAX_SOURCE_AGE` away from the
///   current slot in either direction (landed too late to be fresh, or a
///   source stamp too far ahead to be a plausible landing-slot estimate)
///
/// A step beyond `MM_ORACLE_MAX_STEP_PCT_PRECISION` is neither a hard error nor
/// a skip: it is clamped to the cap and written, matching opcode 0, so a feed
/// gap larger than the cap converges over a few writes instead of freezing the
/// oracle (see `apply_mm_oracle_update`). Clamped entries are reported in their
/// own bitmask so a crank feeding diverging prices can see its writes are being
/// altered.
///
/// One `msg!` per non-zero bitmask (rejected, clamped) is emitted, so the happy
/// path pays nothing for logging. Formatted logging measured ~700 CU on the
/// single-market handler's reject paths, which is why it is not emitted per
/// market.
///
/// # Blast radius of the batch size
///
/// Batching couples the markets in a batch on three axes, all of which scale
/// with `n`: a dropped transaction stales every market in it, a structural error
/// on one market discards every other market's write, and the transaction takes
/// a writable lock on every market for the slot, so fills and liquidations on
/// all of them queue behind the crank. None of this is fatal (a missed update
/// degrades to exchange-oracle pricing via `MMOraclePriceData::new`'s freshness
/// fallback, it does not halt the market), but batch size is a cost-versus-
/// coupling dial, not a free win. Sharding a large market set across a few
/// batches is usually better than one maximal batch.
///
/// # Relationship to opcode 0
///
/// The per-market gating is `apply_mm_oracle_update`, shared with opcode 0, so
/// the two handlers cannot drift apart. `native_batch_tests::
/// batch_matches_single_market_handler` pins them to the same accept and reject
/// decisions at the wire level. The deliberate differences are all in the
/// wrappers:
///
/// - **Account order is not a superset of opcode 0's.** Opcode 0 is
///   `[market, signer, state]`; this is `[signer, state, markets..]`, because
///   the variable-length region has to sit last. Both confusions fail closed
///   (opcode-0 order here yields `Unauthorized`; this order into opcode 0
///   yields `InvalidNativeStateAccount`).
/// - **Payload carries a market index** per entry, cross-checked against the
///   account; opcode 0's does not.
/// - **Non-positive price** is skipped here; opcode 0 returns `Err` (in a batch
///   that would destroy every other market's write).
/// - **Market writability** is checked here; opcode 0 leaves it to the runtime.
/// - **Skips and clamps are reported as bitmasks** here; opcode 0 logs each
///   with its values.
pub fn handle_update_mm_oracle_batch_native(accounts: &[AccountInfo], data: &[u8]) -> Result<()> {
    // Slot comes from the Clock sysvar syscall: no clock account, nothing for
    // a caller to forge, one more market fits the transaction.
    let (rejected_mask, clamped_mask) = update_mm_oracle_batch(accounts, data, Clock::get()?.slot)?;

    // One log per non-zero mask, so the happy path pays nothing for logging.
    // Formatted `msg!` measured ~700 CU on the single-market handler's reject
    // paths, which is why this is not emitted per market.
    if rejected_mask != 0 {
        msg!("mm oracle batch: rejected mask {:#x}", rejected_mask);
    }
    if clamped_mask != 0 {
        msg!("mm oracle batch: clamped mask {:#x}", clamped_mask);
    }

    Ok(())
}

/// Body of `handle_update_mm_oracle_batch_native`, split from the syscall so
/// tests can drive the slot directly and assert the exact bitmasks rather than
/// inferring them from market state. Returns `(rejected_mask, clamped_mask)`:
/// bit `i` of the first is set when entry `i` was skipped, bit `i` of the
/// second when entry `i` landed but its price was clamped to the step cap.
fn update_mm_oracle_batch(
    accounts: &[AccountInfo],
    data: &[u8],
    current_slot: u64,
) -> Result<(u64, u64)> {
    let n = validate_mm_oracle_batch_framing(accounts, data)?;

    // Fixed prologue, paid once for the whole batch. Authenticate the state
    // account before any raw byte read (auth.rs invariant).
    let state_account = &accounts[1];
    crate::auth::require_native_account(
        state_account,
        State::DISCRIMINATOR,
        ErrorCode::InvalidNativeStateAccount,
    )?;
    let slot_clock = read_native_state_slot_clock(state_account)?;

    require_mm_oracle_crank(state_account, &accounts[0])?;

    let mut rejected_mask: u64 = 0;
    let mut clamped_mask: u64 = 0;

    for i in 0..n {
        let market_account = &accounts[MM_ORACLE_BATCH_FIXED_ACCOUNTS + i];

        crate::auth::require_native_account(
            market_account,
            PerpMarket::DISCRIMINATOR,
            ErrorCode::InvalidNativePerpMarketAccount,
        )?;
        // A read-only market would make the runtime fail the whole transaction
        // at the end with an opaque "readonly data modified" error. Catch the
        // builder mistake here instead, with a code that names the account.
        require!(
            market_account.is_writable,
            ErrorCode::InvalidNativePerpMarketAccount
        );

        let payload = read_mm_oracle_batch_entry(data, i);

        // `i < n <= MM_ORACLE_BATCH_MAX_MARKETS`, which `const_assert!`s to at
        // most `u64::BITS`, so the shifts are in range.
        match apply_mm_oracle_update(market_account, payload, current_slot, slot_clock)? {
            MmOracleUpdateOutcome::Written { price } => {
                if price != payload.price {
                    clamped_mask |= 1u64 << i;
                }
            }
            MmOracleUpdateOutcome::Skipped(_) => {
                rejected_mask |= 1u64 << i;
            }
        }
    }

    Ok((rejected_mask, clamped_mask))
}

/// Checks the framing of a batch payload and returns the entry count.
///
/// Every later index derives from the count, so nothing is read until the
/// payload length and the account count both agree with it. Opcode 0 slices a
/// fixed payload and panics on malformed input. A handler whose loop bound is
/// caller-supplied must not repeat that.
fn validate_mm_oracle_batch_framing(accounts: &[AccountInfo], data: &[u8]) -> Result<usize> {
    let n = *data
        .first()
        .ok_or(ErrorCode::InvalidNativeInstructionData)? as usize;
    require!(
        n > 0 && n <= MM_ORACLE_BATCH_MAX_MARKETS,
        ErrorCode::InvalidNativeInstructionData
    );
    require!(
        data.len() == 1 + n * MM_ORACLE_BATCH_ENTRY_LEN,
        ErrorCode::InvalidNativeInstructionData
    );
    require!(
        accounts.len() >= MM_ORACLE_BATCH_FIXED_ACCOUNTS + n,
        ErrorCode::InvalidNativeInstructionData
    );

    Ok(n)
}

/// Reads entry `i` of a batch payload.
///
/// In range, because `validate_mm_oracle_batch_framing` pinned the payload
/// length to the entry count and `i` is below that count.
fn read_mm_oracle_batch_entry(data: &[u8], i: usize) -> MmOraclePayload {
    let entry = 1 + i * MM_ORACLE_BATCH_ENTRY_LEN;

    MmOraclePayload {
        expected_market_index: Some(u16::from_le_bytes(
            data[entry..entry + 2].try_into().unwrap(),
        )),
        price: i64::from_le_bytes(data[entry + 2..entry + 10].try_into().unwrap()),
        sequence_id: u64::from_le_bytes(data[entry + 10..entry + 18].try_into().unwrap()),
        source_slot: u64::from_le_bytes(data[entry + 18..entry + 26].try_into().unwrap()),
    }
}

/// Why one per-market update was skipped. Carried in
/// [`MmOracleUpdateOutcome::Skipped`]: the batch handler folds it into the
/// reject bitmask, opcode 0 logs it with its values.
enum MmOracleSkipReason {
    /// `oracle_validity` classifies a stored non-positive price as
    /// `NonPositive` on read, so storing one buys nothing. Opcode 0 pre-checks
    /// this with a hard error, so it only reaches a mask in the batch.
    NonPositivePrice,
    /// Sequence id not strictly greater than the stored one — the crank's
    /// ordinary redundant-send case.
    StaleSequenceId,
    /// Current slot not strictly greater than the stored slot.
    SlotNotAdvanced { stored_slot: u64 },
    /// Fewer slots since the last accepted write than `MM_ORACLE_MIN_WRITE_GAP` allows.
    RecrankGapTooSmall { gap: u64, min_gap: u64 },
    /// Source slot more than `MM_ORACLE_MAX_SOURCE_AGE` from the current
    /// slot in either direction.
    SourceSlotOutOfRange { source_slot: u64 },
}

/// Result of one per-market update attempt. `Written::price` is the price that
/// actually landed, which differs from the incoming price when the step cap
/// clamped it — callers use that to log (opcode 0) or set the clamped bitmask
/// (batch).
enum MmOracleUpdateOutcome {
    Written { price: i64 },
    Skipped(MmOracleSkipReason),
}

/// One market's entry in a native MM oracle payload.
#[derive(Clone, Copy)]
struct MmOraclePayload {
    /// The market the caller says this entry writes. The batch payload names
    /// one per entry, so the account list and the payload can be cross-checked.
    /// Opcode 0 carries no index and passes `None`.
    expected_market_index: Option<u16>,
    price: i64,
    sequence_id: u64,
    source_slot: u64,
}

fn mm_oracle_source_slot_out_of_range(
    slot_clock: SlotClock,
    current_slot: u64,
    source_slot: u64,
) -> bool {
    if source_slot <= current_slot {
        slot_clock.elapsed(source_slot, current_slot) > MM_ORACLE_MAX_SOURCE_AGE
    } else {
        source_slot.saturating_sub(current_slot)
            > MM_ORACLE_MAX_SOURCE_AGE.to_slots(slot_clock.slot_duration_at(current_slot))
    }
}

/// Applies one MM oracle update to an already-authenticated perp market
/// account. The single copy of the per-market gating, shared by opcode 0
/// (`update_mm_oracle`) and the batch handler (opcode 2), so the two cannot
/// drift apart.
///
/// Returns [`MmOracleUpdateOutcome::Written`] with the price that landed (which
/// the step cap may have clamped) or [`MmOracleUpdateOutcome::Skipped`] with
/// the reason. Skips never abort a batch, see
/// `handle_update_mm_oracle_batch_native`'s failure model.
///
/// `expected_market_index` is `Some` for batch entries, whose payload names the
/// market it expects at each account position; a mismatch is a hard error, not
/// a skip, because it means the account list and payload are misaligned.
/// Opcode 0's payload carries no index and passes `None`.
///
/// # Safety contract
///
/// The caller MUST have already passed `market_account` through
/// `crate::auth::require_native_account(.., PerpMarket::DISCRIMINATOR, ..)`.
/// This function `bytemuck`-casts the account data and would otherwise
/// reinterpret caller-chosen bytes as a `PerpMarket`.
///
/// The mutable borrow is scoped to this call, so passing the same market twice
/// in one batch cannot alias: the second occurrence re-borrows cleanly and then
/// falls out on the slot check, because the first occurrence already advanced
/// `mm_oracle_slot` to `current_slot`.
fn apply_mm_oracle_update(
    market_account: &AccountInfo,
    payload: MmOraclePayload,
    current_slot: u64,
    slot_clock: SlotClock,
) -> Result<MmOracleUpdateOutcome> {
    use MmOracleUpdateOutcome as Outcome;

    let mut market_data = market_account.try_borrow_mut_data()?;
    let market_bytes = market_data
        .get_mut(8..8 + std::mem::size_of::<PerpMarket>())
        .ok_or(ErrorCode::InvalidNativePerpMarketAccount)?;
    let perp_market: &mut PerpMarket = bytemuck::from_bytes_mut(market_bytes);

    // Structural, so it runs before any skip condition: when the caller told us
    // which market this entry is for, the account it paired with the entry must
    // agree. A mismatch means the account list and the payload are misaligned,
    // which is a caller bug and must not be silently absorbed.
    if let Some(expected) = payload.expected_market_index {
        require!(
            perp_market.market_index == expected,
            ErrorCode::InvalidNativePerpMarketAccount
        );
    }

    let stats = &mut perp_market.market_stats;

    if let Some(reason) = mm_oracle_skip_reason(stats, payload, current_slot, slot_clock) {
        return Ok(Outcome::Skipped(reason));
    }

    let price = clamped_mm_oracle_price(stats.mm_oracle_price, payload.price)?;

    stats.mm_oracle_slot = current_slot;
    stats.mm_oracle_price = price;
    stats.mm_oracle_sequence_id = payload.sequence_id;

    Ok(Outcome::Written { price })
}

/// Why this update is skipped, or `None` when it may be written.
///
/// Every gate here is an expected runtime condition for a crank that runs near
/// the minimum write gap. A skip leaves the market untouched and never aborts a
/// batch.
fn mm_oracle_skip_reason(
    stats: &MarketStats,
    payload: MmOraclePayload,
    current_slot: u64,
    slot_clock: SlotClock,
) -> Option<MmOracleSkipReason> {
    use MmOracleSkipReason as Skip;

    if payload.price <= 0 {
        return Some(Skip::NonPositivePrice);
    }

    if payload.sequence_id <= stats.mm_oracle_sequence_id {
        return Some(Skip::StaleSequenceId);
    }

    // Ordered before the subtraction below so the slot gap cannot underflow.
    if current_slot <= stats.mm_oracle_slot {
        return Some(Skip::SlotNotAdvanced {
            stored_slot: stats.mm_oracle_slot,
        });
    }

    // Both gates are wall-clock durations expressed in actual slots, so the
    // write rate limit and source-age bound keep their width at any slot
    // duration. Must stay consistent with the `MM_ORACLE_MIN_WRITE_GAP`
    // fallback inside `oracle_validity`.
    let gap = current_slot - stats.mm_oracle_slot;
    // rate limiter: round the min accepted interval UP so the wall-clock gap is
    // never shorter than intended (floor would loosen the slew cap at intermediate
    // gates). The immediate-fill staleness fallback in `oracle_validity` ceils the
    // same constant, so the accept threshold there matches this write gate exactly.
    let slot_duration = slot_clock.slot_duration_at(current_slot);
    let min_gap = MM_ORACLE_MIN_WRITE_GAP.to_slots_ceil(slot_duration);
    if gap < min_gap {
        return Some(Skip::RecrankGapTooSmall { gap, min_gap });
    }

    // Source-observation freshness around the landing slot.
    // `mm_oracle_slot` is stamped with the landing slot, so a late-landing
    // signed update would otherwise make an old observation read as fresh.
    // The bound applies in both directions: a source slot far in the future is
    // a caller bug (a wrong-unit value, e.g. a millisecond timestamp, would
    // otherwise disable this gate permanently and silently), while a small
    // forward allowance still lets a crank estimate its landing slot.
    // Past observations use the piecewise clock so a transition cannot disguise
    // old source time. A small future landing estimate has no elapsed interval
    // yet, so it keeps the conservative endpoint conversion.
    if mm_oracle_source_slot_out_of_range(slot_clock, current_slot, payload.source_slot) {
        return Some(Skip::SourceSlotOutOfRange {
            source_slot: payload.source_slot,
        });
    }

    None
}

/// Holds one write to the step cap against the last accepted price.
///
/// A step beyond the cap is clamped to the cap rather than skipped, so a feed
/// gap larger than the cap converges over a few writes instead of freezing the
/// oracle at its pre-gap price. The cap has a floor of one price unit, so a
/// price small enough for the cap to round to zero still makes progress. A
/// stored price of zero is a market that is still bootstrapping, and it takes
/// the incoming price as it is.
fn clamped_mm_oracle_price(stored_price: i64, incoming_price: i64) -> Result<i64> {
    if stored_price == 0 {
        return Ok(incoming_price);
    }

    let prev = stored_price as i128;
    let max_step = MM_ORACLE_MAX_STEP_PCT_PRECISION
        .saturating_mul(prev.abs())
        .saturating_div(PERCENTAGE_PRECISION_I128)
        .max(1);
    let diff = (incoming_price as i128).saturating_sub(prev);
    if diff.abs() <= max_step {
        return Ok(incoming_price);
    }

    // Between `prev` and `incoming_price`, so always i64-representable.
    Ok(prev
        .saturating_add(max_step.saturating_mul(diff.signum()))
        .cast::<i64>()?)
}
