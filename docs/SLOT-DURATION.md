# Slot duration and time units

How Velocity handles Solana's slot time reduction (400ms -> 350 -> 300 -> 250 -> 200ms, via the
IBRL feature gates). This covers where the current slot length lives, how wall-clock rules keep
their meaning at every gate, and the type system that keeps new code from assuming a slot length
again.

Solana's slot counter is the only clock a program can read cheaply, so every time-based rule in this
codebase was originally written as a slot count under the assumption that one slot takes 400ms.
"Stale after ~5 seconds" became `10`, "a one-minute liquidation ramp" became `150`, "idle after an
hour" became `9000`. The number and the meaning were fused, and the fusion was only valid while the
assumption held. As the gates activate, a raw slot count changes meaning with no error and no event.
Oracle staleness windows tighten until fills and liquidations revert, user-protection ramps halve,
and per-slot rate limits double in throughput. The stored numbers keep their value and start
denominating something else.

The design separates four concerns and gives each one mechanism:

| Concern | Mechanism | Where |
| --- | --- | --- |
| What is a slot worth at any slot | `State.slot_duration_transition_slots`, synchronized permissionlessly per gate | [`state.rs`](../programs/velocity/src/state/state.rs) |
| Elapsed intervals crossing a gate stay exact | `SlotClock` integrates each regime piecewise | [`math/time.rs`](../programs/velocity/src/math/time.rs) |
| Existing rules keep their wall-clock meaning | durations convert to actual slots at the read site | same module |
| Future code cannot recreate the bug | wall-clock durations are a type (`Millis`), not a number | same module |

The entire slot transition is therefore a data change. Each gate activation takes one
permissionless sync instruction. There are no constant retunes and no redeploys, and anyone who
later compares a duration against a slot count without converting gets a compile error.

## The clock: `State.slot_duration_transition_slots`

The slot clock is held by `State` fields carved in place from the padding after `promo_fee_tier`.
`const_assert_eq!` and the native-offset guard test pin the offsets.

- `slot_duration_transition_slots` (`[u64; 4]`, bytes 1520..1552) holds the first slot of each
  post-baseline regime, ordered `[350ms, 300ms, 250ms, 200ms]`. `0` means that transition has not
  been synchronized yet. This archive is the authoritative clock. It answers both "what is a slot
  worth at slot N" (`SlotClock::slot_duration_at`) and "how much wall-clock time passed between two
  slots" (`SlotClock::elapsed`, integrated per regime).
- The legacy staging trio remains as a fallback for accounts written by the first implementation
  and for older readers. `slot_duration_ms` (`u16`, bytes 1506..1508; `0` = unset = 400ms baseline,
  so there was no migration), `pending_slot_duration_ms` (`u16`, bytes 1508..1510), and
  `slot_duration_effective_slot` (`u64`, bytes 1512..1520). The sync instruction keeps these
  coherent, but once any archive entry exists the archive wins.

Never read the fields directly. `State::slot_clock()` returns the full clock, and
`State::slot_duration()` resolves the live duration from it against the Clock sysvar, so the live
duration changes at each boundary without a transaction timed to that boundary. The resolution
lives in one place, `math::time::SlotClock`, shared by `slot_duration()`, the native fast-path
reader, the foreign-account reader vaults uses, and the off-chain mirrors.

The writer is `sync_state_slot_duration` (permissionless; CLI:
`velocity-admin exchange sync-slot-duration <ms>`). Anyone may crank it once per gate, and the
caller supplies no timing truth.

1. Fixed gate set. The accounts struct constrains the feature account to the feature-gate
   program and to one of the four scheduled IBRL keys. The target duration is derived from the key
   (`ibrl_slot_duration_ms`), so there is nothing to typo. Recorded transition slots must be
   monotonic. A forward sync may skip an abandoned gate after the last recorded boundary is live,
   and a missing older gate may be backfilled later. Neither path may make the active duration
   slower. Re-syncing an already-recorded gate is an idempotent no-op that revalidates the recorded
   slot against the feature account.
2. Effective slot derived from the gate and the `EpochSchedule` sysvar. The handler verifies
   the feature account is activated (`data[0] == 1`), reads its activation slot, and computes
   `epoch_schedule.get_first_slot_in_epoch(get_epoch(activated_at) + 1)`, exactly Agave's own
   feature-activation arithmetic, correct for warmup epochs and for a mid-epoch `activated_at`. It
   does not require the following epoch to have begun, because recording the boundary in advance is
   the point. Everything the handler accepts is fixed by the feature account and the sysvar, so
   admin authorization would add a failure mode (a missed multisig) without adding security. That
   is why the instruction is permissionless.

Lockstep matters because there is no universally-safe direction for a mismatch between the live
`slot_duration_ms` and the real slot length. A conversion serves both risk *ceilings* (oracle
staleness, where a shorter window is safer) and user-protection *minima* (liquidation ramps and
grace periods, where a longer window is safer), and the two want opposite errors. With max allowed
real oracle age `= (window_ms / live_ms) x real_slot_ms`:

- Live value ahead of the chain (200ms while the chain is still 400ms). A 48s oracle window
  becomes `48000/200 x 400 = 96s`, so it *widens* and accepts staler oracles, which is an unsafe
  ceiling. Ramps meanwhile *lengthen*, which is safe for the user.
- Live value behind the chain (400ms while the chain is already 200ms). The oracle window
  becomes `48000/400 x 200 = 24s`, so it *tightens*, a safe ceiling at a liveness cost. But ramps
  and grace periods *shorten* to half their intended wall-clock, which is an unsafe minimum.

Synchronizing during the warmup makes the effective value match the chain at the exact boundary, so
neither lag is ever live in steady state. That is the whole point of deriving the effective slot
from the gate rather than requiring a hand-timed transaction at the boundary.

Runbook, in full:

```
# once the Feature Gate Tracker shows the gate activated (any signer; keepers can crank it):
velocity-admin exchange sync-slot-duration 350   # then 300, 250, 200 as each is activated
```

Each transition needs only its one sync transaction, ideally during the activation epoch. State
switches automatically at the effective slot. A late sync still records the exact historical
boundary, because the feature account keeps its activation slot forever, so elapsed-time math is
repaired retroactively. The only cost of lateness is that the live duration lagged in the interim.
No guard-rail retunes, per-market updates, or bot restarts are needed, since the off-chain mirrors
read the same fields from their state subscription.

For a cluster whose legacy staging fields already reached a faster regime before this archive is
initialized, synchronize the currently active fastest gate first, then backfill older gates. For
example, a cluster already at 200ms must sync 200ms before 350/300/250ms. The handler checks the
proposed clock at the current slot and rejects any archive write that would move it backward, so a
permissionless caller cannot temporarily restore a slower duration. The legacy staging trio is
then rebuilt from the archive rather than allowed to block canonical repair.

### Adding another slot-duration transition

The clock math and elapsed-time call sites already support piecewise regimes. A future reduction,
for example to 120ms, requires extending the transition-duration list and State archive capacity,
adding the canonical feature key mapping, updating the native and foreign-account byte readers,
and mirroring the new field and constant in TypeScript and Rust clients. Derive its boundary from
the feature account plus `EpochSchedule`. Do not add a hardcoded epoch length or an admin setter.
If no padding remains, this is a State version/layout migration rather than permission to overload
a legacy field. Add parity tests for intervals and forward deadlines crossing the new boundary.

### Measurements that straddle a switch: the piecewise clock

Solana's slot *counter* does not record that earlier slots were longer, so converting a whole
measured delta at one endpoint duration mis-times any interval that crosses a transition. The
result is an under-count, which is safe for elapsed/ramp sites (it favors the user) and unsafe for
oracle-freshness gates (it briefly accepts a slightly staler oracle).

The transition archive removes that residual. `SlotClock::elapsed(start_slot, end_slot)` walks the
recorded transitions and sums each segment at its own duration, so an interval spanning one or all
four gates converts to its exact wall-clock length. `SlotClock::elapsed_slot_delta(delta, end_slot)`
is the same walk for the "measured delta ending now" shape most call sites have. Every elapsed-time
rule in the program measures through it, including oracle ages (`oracle_validity`), the AMM
staleness gate, liquidation fee and ramp (`get_liquidation_fee`, `calculate_max_pct_to_liquidate`),
the filler time-reward curve, idle/eviction/rest windows, and the VLP hedge uncertainty fees.
Forward-looking conversion of a wall-clock window into a slot bound uses
`SlotClock::slot_at_or_after_duration`, which walks known future boundaries. The signed-message
order's `max_slot` therefore keeps the full `SIGNED_MSG_FILL_WINDOW` even when its placement window
crosses a synchronized transition.

Without any archive entry (pre-sync accounts, hand-built test states), `SlotClock` falls back to
the legacy staging fields and prices the whole delta at the end-slot duration. That is the previous
behavior, and it is the identity at the 400ms baseline.

## The types

All of it lives in [`math/time.rs`](../programs/velocity/src/math/time.rs). The TypeScript mirror
is [`math/time.ts`](../packages/sdk/src/math/time.ts), with the same names, the same rounding, and
branded compile-time-only types.

`Millis` is the only duration unit in the codebase. Every wall-clock threshold, window, ramp,
and grace period is a `Millis`, whether it comes from a code constant or an admin-set field. It
compares only against other `Millis`, and the compiler rejects any comparison or arithmetic against
a raw slot count. Constructors and conversions:

```rust
Millis::from_secs(600)                 // a code constant: ten minutes, readable as written
Millis::from_ms(800)                   // sub-second constants
Millis::from_slots(delta, d)           // the exact wall-clock time a measured slot delta represents

m.to_slots(d)                          // express in actual slots, floor
m.to_slots_ceil(d)                     // express in actual slots, ceil (user-protection windows)
m.div_periods(Millis::UNIT)            // count whole 400ms periods (legacy per-slot rates)
```

`SlotClock` is the cluster clock, the four transition slots plus the legacy staging fields. It is
built only from `State` (`State::slot_clock()`, `SlotClock::from_state_fields`). It answers
`slot_duration_at(slot)` for any slot and integrates `elapsed(start, end)` /
`elapsed_slot_delta(delta, end)` piecewise across the recorded regimes. Every measured-interval
site takes a `SlotClock`. `SlotClock::baseline()` is the all-zero 400ms clock for tests and
contexts with no `State`.

`SlotDuration` is one slot length. Program logic obtains it from the clock
(`slot_clock.slot_duration_at(slot)` or the `State::slot_duration()` shorthand). There is
deliberately no constructor from an arbitrary number in program logic, so a slot *count* can never
be passed where the slot *length* belongs. Both directions of that mixup were previously bare
`u64`s sitting next to each other in thirty function signatures. `SlotDuration` remains the
parameter type for forward-looking window conversions (`to_slots` / `to_slots_ceil`).
`SlotDuration::BASELINE` (400ms) exists for tests and for contexts with no `State` account, which
by construction also run on default guard rails.

`StoredSlotDuration<T, const SLOT_MS: u64>` is the compact account-storage type. `T` fixes the
wire width and `SLOT_MS` records the slot length assumed when that field was created. So
`StoredSlotDuration<u8, 400>` still occupies one byte, but a raw `10` unambiguously means 4,000ms,
and `StoredSlotDuration<u8, 200>` with the same raw byte means 2,000ms. Program logic calls
`to_millis()` immediately and performs all arithmetic in `Millis`:

```rust
type LegacyDuration = StoredSlotDuration<u8, 400>;
let stored = LegacyDuration::try_from_millis(Millis::from_ms(4_000)).unwrap();
assert_eq!(stored.raw_units(), 10);
assert_eq!(stored.to_millis(), Millis::from_ms(4_000));

LegacyDuration::try_from_millis(Millis::from_ms(4_001)); // None: not an exact 400ms multiple
```

The type is `repr(transparent)` and preserves the wrapped integer's bytes, size, and alignment.
The account aliases expose their primitive wire types during IDL generation because Anchor's
JavaScript coder does not yet flatten transparent generic wrappers, so SDK users keep the
existing `u8`/`i64`/`u64` decoded shapes. Signed fields with sentinel values are intentionally not
modeled as ordinary durations and continue through `DelayOverride`.

Changing a field from (for example) `StoredSlotDuration<u8, 400>` to
`StoredSlotDuration<u8, 200>` changes the interpretation of every existing byte. That is a real
data migration. Coordinate the program upgrade with an admin rewrite that preserves each field's
wall-clock value. Never change only the const parameter.

`DelayOverride` decodes the per-market `i8` oracle-delay overrides. It moves the `0` and negative
sentinel branching out of `oracle_validity` and into two constructors
(`DelayOverride::from_immediate`, `DelayOverride::from_low_risk`), so the sentinel scheme is a
type instead of an if-chain.

Plain `u64` remains the type of actual slot counts, and genuine chain-slot logic never touches
`Millis`. That covers same-slot idempotence checks, blockhash validity windows, and "a fill lands
~1 slot ahead" estimates. The two kinds of numbers have different types and cannot be compared or
combined without an explicit conversion.

## How a rule survives the gates

Take the signed-msg eviction grace as the worked example:

```rust
pub const SIGNED_MSG_EVICTION_BUFFER: Millis = Millis::from_ms(4_000);
// at the use site: the measured age, integrated per regime, against the intent
let expired = slot_clock.elapsed(existing.max_slot, current_slot) > SIGNED_MSG_EVICTION_BUFFER;
```

The author's decision is "give relayers ~4 seconds of grace". The constant states that decision
directly, and the measured age floats with the chain. Ten elapsed slots read as 4.0s at 400ms and
2.0s at 200ms, and an interval crossing a gate sums each side at its own duration.

At the baseline every conversion is the identity on the historical values (10 slots = 4,000ms,
exactly the old constant), which is why the refactor changed no test expectation. The program's
unit suite passes unmodified, which shows the type system is pure structure with zero behavior
change until the first gate flips.

Here is the compile-time half, for the same constant. All of these were expressible in the
raw-number era, and one of them was the shipped code. None of them compile now:

```rust
max_slot + SIGNED_MSG_EVICTION_BUFFER                       // u64 + Millis: type error
slot_delta > SIGNED_MSG_EVICTION_BUFFER                     // u64 vs Millis: type error
SIGNED_MSG_EVICTION_BUFFER.to_slots(current_slot)           // expected SlotDuration: type error
```

Only the correct form type-checks. A developer who has never heard of the 400ms era does not need
to know the conversion functions exist. The type error names the expected type, and the method that
produces it takes a `SlotDuration`, which is only obtainable from `State`.

## Rounding

Rounding direction is chosen per conversion and mirrored exactly by the TypeScript SDK. Do not
change one side without the other.

| Conversion | Rounding | Why |
| --- | --- | --- |
| `to_slots` (staleness windows, rate limits) | floor | a marginally tighter window is the safe failure direction |
| `to_slots_ceil` (forward slot bounds: the signed-msg `max_slot`, the MM-oracle write gate) | ceil | the user never gets less than the intended time |
| `from_slots` (measured deltas) | exact | multiplication, no rounding |
| `div_periods` (elapsed time into rate periods) | floor | elapsed time is under-counted, so fee ramps and expiries engage marginally later, favoring the affected user |

The worst-case rounding error at any gate is under one slot. For example, a 4s window is 11 slots =
3.85s at 350ms instead of 11.43 slots.

## Legacy stored fields and the 400ms encoding

Code constants were freely rewritten in milliseconds, since they are not stored anywhere. Admin-set
onchain fields could not be. They hold live values on mainnet, and some are too narrow to hold
milliseconds at all (`liquidation_duration` is a `u8`, and 60 seconds is 60,000). Those fields keep
their compact encoding in units of `STORED_UNIT_MS` = 400ms, the historical slot length, and the
encoding is confined to each field's typed getter:

| Stored field | Storage | Getter |
| --- | --- | --- |
| `ValidityGuardRails.slots_before_stale_for_amm` / `_for_margin` | `StoredSlotDuration<i64, 400>` (IDL: `i64`) | `stale_for_amm_ms()` / `stale_for_margin_ms()` -> `Millis` |
| `State.liquidation_duration` | `StoredSlotDuration<u8, 400>` (IDL: `u8`) | `liquidation_duration_ms()` -> `Millis` |
| `State.min_perp_auction_duration` | `StoredSlotDuration<u8, 400>` (IDL: `u8`) | `min_perp_auction_duration_ms()` -> `Millis` |
| `PerpMarket.oracle_slot_delay_override` / `oracle_low_risk_slot_delay_override` | `i8` with sentinels, 400ms units | `DelayOverride::from_immediate` / `from_low_risk` |
| `Constituent.oracle_staleness_threshold` (VLP hedge) | `StoredSlotDuration<u64, 400>` (IDL: `u64`) | normalized to `Millis` at use |

The `400` is a storage codec detail, the same way nobody "thinks in" `PRICE_PRECISION`. Admins
type seconds in the CLI, which encodes on the way in. Logic compares `Millis`. The chain stores a
compact integer. New compact stored durations should encode their chosen quantum in
`StoredSlotDuration<T, SLOT_MS>`; use a native millisecond integer instead when arbitrary
millisecond precision matters more than width. Replacing a legacy field's encoding follows the same
steps used elsewhere in this codebase. Carve a ms-native replacement field from padding, give the
getter a fallback, re-set the value once, and the typed choke point means nothing else in the
codebase changes.

A few legacy *rates* were calibrated per-slot in the 400ms era and keep that period explicitly.
The liquidation fee accrues `LIQUIDATION_FEE_INCREASE_PER_PERIOD` per `Millis::UNIT` (400ms) of
elapsed time, the reference-price-offset smoothing budget and the VLP hedge uncertainty-fee bucket
count the same periods. The period is visible at those sites on purpose, because it is part of
the tuned economics and changing it changes the rate.

## Order age and rest windows

Order auctions are removed. `Order.unused_auction_duration` keeps the old byte so the layout does
not change, and nothing reads it. `State.min_perp_auction_duration` keeps its 400ms encoding but
has no reader.

`BID_ASK_TWAP_MIN_QUOTE_REST` (~9.6s) is measured piecewise from the same clock as the
`SafeTriggerOrder` horizon. A quote must rest that long before the bid/ask and mark TWAPs read it
(OtterSec #146), and that rule holds at every gate.

One related fact does not scale. `Order.posted_slot_tail` is a mod-256 slot stamp, a field width
rather than a constant, so the honest order-age window it can express shrinks in wall-clock as
slots get faster (102s at 400ms, 51s at 200ms). The rest requirement (48 actual slots at 200ms)
still fits under it.

## What deliberately does not convert

Chain-slot logic where the slot is the unit of interest, correct at any slot speed:

- same-slot idempotence (`amm.last_update_slot == slot`, per-slot AMM projection, MM-oracle
  monotonicity)
- blockhash validity offsets and landing-slot estimates in the bots
- the `posted_slot_tail` modulus (field width)
- bot liveness tripwires counted in slot ticks (a dead-feed restart firing sooner at faster slots
  is acceptable, and each site documents it)

And one legacy oddity kept for compatibility. `block_operation`'s funding gate compares elapsed
wall-clock milliseconds against `funding_period * 400`, where `funding_period` is denominated in
seconds. The pre-existing code compared a raw slot count against the seconds value, a unit mismatch
that widened with faster slots. The gate now holds its current wall-clock width (~40% of the
funding period) at every slot duration.

### Paths that run on the baseline (no `State` in scope)

A few instruction paths load oracle validity without a velocity `State` account in scope and so
decode the guard-rail staleness windows at the 400ms baseline regardless of the live slot duration.
Those are the two `UpdateUser` handlers (margin-trading toggle, pool-id). Vault instructions,
including `manager_update_borrow`, resolve the full clock from their `velocity_state` account via
`State::slot_clock_from_account_info`, which validates owner and discriminator and reads the archive
and staging fields by offset. Floor-tightening is the safe direction, since a 4s window becomes 2s
at 200ms, stricter and never more permissive. The remaining baseline paths are therefore a liveness
note, not a safety gap. At 200ms they want an oracle cranked within ~2s. keep-rs's liquidator and
filler re-sample the slot duration on a live cadence. The liquidator's rate limiter reads a shared
value the main loop refreshes, and the filler refreshes on an elapsed-slot config tick, so a process
spanning a gate activation picks up the new duration without a restart.

## Writing new code

The rules reduce to one decision. Is the value about wall-clock time, or about slots as slots?

```rust
// a wall-clock rule: write the time you mean, measure through the clock
const REBALANCE_COOLDOWN: Millis = Millis::from_secs(30);
if state.slot_clock().elapsed(last_rebalance, slot) >= REBALANCE_COOLDOWN { ... }

// genuine slot logic: plain u64, no conversion, no Millis
if amm.last_update_slot == slot { return Ok(()); }   // same-slot idempotence

// a compact admin-set duration: the field type records its storage quantum
pub cooldown: StoredSlotDuration<u16, 200>,
let cooldown = self.cooldown.to_millis();

// if arbitrary millisecond precision matters more than compactness, store ms directly
pub cooldown_ms: u32,
let cooldown = Millis::from_ms(self.cooldown_ms as u64);
```

Getting it wrong does not compile. A raw threshold will not compare against anything the rest of
the system produces, since measured deltas arrive as `Millis` and thresholds are consumed as
`Millis`. The resulting type error leads to `math/time.rs`, whose module doc states this table.

## Off-chain mirrors

Everything off chain reads the same `State` slot-duration fields from its existing state
subscription. No service needs a restart at a gate flip.

- SDK ([`math/time.ts`](../packages/sdk/src/math/time.ts)). Branded `Millis` (a `BN`) and
  `SlotDurationMs` (a `number`), zero runtime cost. `activeSlotDurationFromState` consults the
  transition archive first and falls back to the legacy staging fields, and `elapsedMillis` /
  `elapsedMillisFromSlotDelta` mirror `SlotClock::elapsed` piecewise. The program mirrors that
  measure elapsed time (`getOracleValidity`, `isOracleValid`, `getSpotOracleValidity`,
  `blockOperation`, `getLiquidationFee`, `calculateMaxPctToLiquidate`, `User.canMakeIdle`) take an
  optional trailing `SlotDurationState`, which is the decoded `State` or its slot-duration fields.
  It defaults to the baseline, so state-less callers stay correct until the first gate.
  `SLOT_TIME_ESTIMATE_MS` is deprecated. There is no correct constant to replace it with, only the
  live fields. `currentSlotDuration` / `currentSlotClock` resolve the live duration for any client
  holding a subscribed `State` (see [Off-chain clients](#off-chain-clients-sdk-common-ts-ui-bots)).
- Bots (`apps/keeper-bots-v2`). Operator config knobs are wall-clock ms. A bot that needs the live
  duration resolves it through the SDK's `currentSlotDuration(client, currentSlot)` helper. Pass a
  live chain slot so a switch is applied. Unavailable state or slot falls back to the 400ms
  baseline. `apps/dlob-server` reads no slot duration.
- Rust bots (`rust/keep-rs`, `rust/swift`, `rust/velocity-rs`). These use the program's `SlotClock`
  directly through `velocity_rs::slot_clock_from_state` / `VelocityClient::slot_clock`. keep-rs's
  oracle-validity gates and liquidation-fee mirror, the AMM quoting projection, and the swift local
  margin simulation all measure through it, while forward window conversions keep the live
  `SlotDuration` (`VelocityClient::slot_duration_at`).

### Off-chain clients (SDK, common-ts, UI, bots)

Every TypeScript client resolves live slot lengths through the same `State` fields. No client
holds a slot length of its own, not a constant and not a config value. (The math helpers listed
above still default their trailing `SlotDurationState`/`slotDuration` to the baseline so existing
callers keep compiling. A client that has a subscribed `State` should always pass it rather than
take the default.)
`SLOT_TIME_ESTIMATE_MS` stays exported and deprecated for one minor series only so consumers can
bump without a flag day. There is no correct constant to replace it with.

The entry point for any client holding a subscribed `State` is
`currentSlotDuration(source, currentSlot)` in
[`math/time.ts`](../packages/sdk/src/math/time.ts). `currentSlotClock` returns the same value plus
an `isLive` flag. `source` is duck-typed on `{ getStateAccount() }`, so anything holding a
subscribed `State` works, and `math/time` keeps importing only `BN`.

Code that already has a decoded `State` and a slot in hand, such as `velocityClient`, `user.ts`,
`adminClient` and `cli-admin`, calls the underlying
`activeSlotDurationFromState(state, slot)` instead. That is the same staged-flip resolution
without the subscription lookup or the fallback, so those sites must handle an absent `State`
themselves. Use the resolver whenever the state comes from a subscription that may not be
ready. The resolver enforces two rules so callers cannot get them wrong.

- `currentSlot` must be the live chain slot, not the slot `State` was last written at. `State`
  does not change at the gate boundary, so a cached State slot would never trigger the staged
  switch.
- A missing or `0` slot is a dead feed, not slot zero. A failed slot subscription reports `0`,
  and slot `0` precedes every effective slot, so treating it as live would return the pre-flip base
  while looking correct. The resolver returns the hardcoded 400ms `SLOT_DURATION_BASELINE` and
  `isLive: false` instead.

The fallback is the baseline, and it is not chosen per call site. Unavailable state or slot falls
back to 400ms, the longest scheduled slot. Callers never pass a fallback. Which direction that errs
in is set by the conversion, not by the kind of caller:

| Conversion | Effect of the 400ms fallback on a 200ms chain | Typical call site |
| --- | --- | --- |
| ms to slots (`msToSlotsNum`, `msToSlotsCeilNum`) | fewer slots, so the window closes sooner | staleness gates, rate limits |
| slots to ms (`millisFromSlots`, a bare multiply) | up to 2x more ms, so the window stays open longer | countdowns, cache TTLs, signing budgets |

"Fewer slots" is the safe side for a threshold you want to be conservative under, and the wrong
side for a countdown you are showing a user. "More ms" is always the wrong side for anything the
user relies on to still be valid. At a real 200ms, telling someone they have 400ms per slot
promises twice the wall clock they have, and they keep signing an already-expired transaction.

So a call site in the slots-to-ms direction, and any user-protection window in either direction,
must branch on `isLive` and substitute `SLOT_DURATION_FLOOR` (200ms, the shortest scheduled slot)
rather than consume `slotDurationMs` blindly. `SLOT_DURATION_SCHEDULE_MS` mirrors the program's
full schedule for callers that need the whole ladder. The UI's `useSlotClock` hook is a worked
example. It exposes `useUserProtectionSlotClock` and
`useRiskCeilingSlotClock` over this API.

Program mirrors convert exactly as the program does. Where a client reproduces an onchain
computation, mirror the program's arithmetic step for step. Use the same `Millis` intent, the same
clamps, the same rounding direction, and the same caps that the stored field widths impose. A mirror
that floors where the program ceils, or that skips a cap, makes the client predict a different
result than the chain grants.

## Quick reference

| Thing | Value |
| --- | --- |
| Clock | `State.slot_duration_transition_slots` (`[u64; 4]`, bytes 1520..1552; `0` = unsynced); legacy staging trio as fallback |
| Writer | `sync_state_slot_duration`, permissionless; gate keys fixed on the accounts struct, effective slot from `EpochSchedule` |
| CLI | `velocity-admin exchange sync-slot-duration <ms>` |
| Duration types | `Millis` for arithmetic; `SlotClock` for measured intervals; `StoredSlotDuration<T, SLOT_MS>` for compact account storage |
| Slot length type | `SlotDuration` / `SlotDurationMs`, sourced from the clock (`State::slot_duration()`) |
| Legacy encoding | `STORED_UNIT_MS` = 400, carried in the stored fields' Rust types and normalized to `Millis` |
| Legacy rate period | `Millis::UNIT` (400ms), explicit at each rate site |
| Ops | one permissionless sync per gate, any time at or after activation (during the activation epoch keeps the live value lockstep) |
| Behavior at 400ms | identity, since every conversion reproduces the historical slot counts exactly |
