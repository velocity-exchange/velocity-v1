# Slot duration and time units

How Velocity handles Solana's slot time reduction (400ms -> 350 -> 300 -> 250 -> 200ms, via the
IBRL feature gates): where the current slot length lives, how wall-clock rules keep their meaning
at every gate, and the type system that keeps new code from ever assuming a slot length again.

Solana's slot counter is the only clock a program can read cheaply, so every time-based rule in
this codebase was originally written as a slot count under the assumption that one slot takes
400ms: "stale after ~5 seconds" became `10`, "a one-minute liquidation ramp" became `150`, "idle
after an hour" became `9000`. The number and the meaning were fused, and the fusion was only valid
while the assumption held. As the gates activate, a raw slot count silently changes meaning:
oracle staleness windows tighten until fills and liquidations revert, user-protection ramps halve,
auctions finish in half the intended time, per-slot rate limits double in throughput. Nothing on
chain announces the change; the numbers just start meaning something else.

The design separates three concerns and gives each one mechanism:

| Concern | Mechanism | Where |
| --- | --- | --- |
| What is a slot worth right now | `State.slot_duration_ms`, admin-set once per gate | [`state.rs`](../programs/velocity/src/state/state.rs) |
| Existing rules keep their wall-clock meaning | durations convert to actual slots at the read site | [`math/time.rs`](../programs/velocity/src/math/time.rs) |
| Future code cannot recreate the bug | wall-clock durations are a type (`Millis`), not a number | same module |

The result: the entire slot transition is a data change. One admin instruction per gate
activation, no constant retunes, no redeploys, and a compile error for anyone who later tries to
compare a duration against a slot count without converting.

## The knob: `State.slot_duration_ms`

`State.slot_duration_ms` is a `u16` carved in place from the padding after `promo_fee_tier`
(bytes 1506..1508 of the account data; the offset is pinned by a `const_assert_eq!` and the
native-offset guard test). Pre-upgrade accounts read `0` out of former padding, which means
"unset" and resolves to the 400ms baseline, so there was no migration. Never read the field
directly: `State::slot_duration()` returns the resolved [`SlotDuration`](#the-types).

The setter is `update_state_slot_duration_ms` (warm admin; CLI:
`velocity-admin exchange set-slot-duration-ms <ms>`). It accepts only the feature-gate values
`{350, 300, 250, 200}` (`VALID_SLOT_DURATIONS_MS`), and only strictly below the current effective
value. Both guards exist for the same reason: the one catastrophic operator error would be setting
the field back to a larger duration on a fast chain, which would silently shrink every wall-clock
safety window at once (a 4s oracle window becomes 2s, a 60s liquidation ramp becomes 30s). Feature
gates cannot deactivate, so slots never get slower again, and the monotonic guard makes the error
unrepresentable rather than merely reviewed for. Flipping a gate value *early* (before the chain
gate activates) errs in the lenient direction: windows temporarily widen, nothing loses liveness.

Gate activation runbook, in full:

```
# when the Feature Gate Tracker shows the next IBRL gate activated on mainnet
velocity-admin exchange set-slot-duration-ms 350   # then 300, 250, 200 as each lands
```

Nothing else. No guard-rail retunes, no per-market updates, no bot restarts (the off-chain
mirrors read the same field from their state subscription).

## The types

All of it lives in [`math/time.rs`](../programs/velocity/src/math/time.rs) (TypeScript mirror:
[`math/time.ts`](../packages/sdk/src/math/time.ts), same names, same rounding, branded
compile-time-only types).

**`Millis`** is the only duration unit in the codebase. Every wall-clock threshold, window, ramp,
and grace period is a `Millis`, whether it comes from a code constant or an admin-set field. It
compares only against other `Millis`; the compiler rejects any comparison or arithmetic against a
raw slot count. Constructors and conversions:

```rust
Millis::from_secs(600)                 // a code constant: ten minutes, readable as written
Millis::from_ms(800)                   // sub-second constants
Millis::from_stored_units(raw)         // decode a legacy stored field (400ms units, codec only)
Millis::from_slots(delta, d)           // the exact wall-clock time a measured slot delta represents

m.to_slots(d)                          // express in actual slots, floor
m.to_slots_ceil(d)                     // express in actual slots, ceil (user-protection windows)
m.div_periods(Millis::UNIT)            // count whole 400ms periods (legacy per-slot rates)
```

**`SlotDuration`** is the live slot length. Its only constructor from chain data is
`SlotDuration::from_state_ms` (used by `State::slot_duration()` and, byte-decoded, by the native
MM-oracle handlers); there is deliberately no constructor from an arbitrary number in program
logic, so a slot *count* can never be passed where the slot *length* belongs. Both directions of
that mixup were previously bare `u64`s sitting next to each other in thirty function signatures.
`SlotDuration::BASELINE` (400ms) exists for tests and for contexts with no `State` account, which
by construction also run on default guard rails.

**`DelayOverride`** decodes the per-market `i8` oracle-delay overrides, moving the `0` / negative
sentinel branching out of `oracle_validity` and into two constructors
(`DelayOverride::from_immediate`, `DelayOverride::from_low_risk`), so the sentinel scheme is a
type instead of an if-chain.

Plain `u64` remains the type of actual slot counts, and genuine chain-slot logic never touches
`Millis`: same-slot idempotence checks, blockhash validity windows, "a fill lands ~1 slot ahead"
estimates, and the per-order `auction_duration` snapshot (see
[Auctions](#auctions-and-the-u8-ceiling)). Two kinds of numbers exist and the type system keeps
them apart rather than flattening them.

## How a rule survives the gates

Take the signed-msg eviction grace as the worked example:

```rust
pub const SIGNED_MSG_EVICTION_BUFFER: Millis = Millis::from_ms(4_000);
// at the use site:
let eviction_buffer = SIGNED_MSG_EVICTION_BUFFER.to_slots(slot_duration);
```

The author's decision is "give relayers ~4 seconds of grace". The constant states that decision
directly; the slot expression of it floats with the chain:

| Chain slot time | `to_slots` gives | real grace |
| --- | --- | --- |
| 400ms | 10 slots | 4.0s |
| 350ms | 11 slots | 3.85s |
| 300ms | 13 slots | 3.9s |
| 250ms | 16 slots | 4.0s |
| 200ms | 20 slots | 4.0s |

At the baseline every conversion is the identity on the historical values (4000/400 = 10, exactly
the old constant), which is why the refactor changed no test expectation: the program's unit
suite passes unmodified, and that is the proof the type system is pure structure with zero
behavior change until the first gate flips.

The compile-time half of the story, for the same constant. All of these were expressible (and one
of them was the shipped code) in the raw-number era; none of them compile now:

```rust
max_slot + SIGNED_MSG_EVICTION_BUFFER                       // u64 + Millis: type error
slot_delta > SIGNED_MSG_EVICTION_BUFFER                     // u64 vs Millis: type error
SIGNED_MSG_EVICTION_BUFFER.to_slots(current_slot)           // expected SlotDuration: type error
```

The only sentence the compiler accepts is the correct one. A developer who has never heard of the
400ms era does not need to know the conversion functions exist; the type error walks them to the
method, and the method's signature demands the `SlotDuration` they can only get from `State`.

## Rounding

Rounding direction is chosen per conversion and mirrored exactly by the TypeScript SDK. Do not
change one side without the other.

| Conversion | Rounding | Why |
| --- | --- | --- |
| `to_slots` (staleness windows, rate limits) | floor | a marginally tighter window is the safe failure direction |
| `to_slots_ceil` (liquidation ramps, grace periods, auction durations) | ceil | the user never gets less than the intended time |
| `from_slots` (measured deltas) | exact | multiplication, no rounding |
| `div_periods` (elapsed time into rate periods) | floor | elapsed time is under-counted, so fee ramps and expiries engage marginally later, favoring the affected user |

The worst-case rounding error at any gate is under one slot (e.g. a 4s window is 11 slots = 3.85s
at 350ms instead of 11.43).

## Legacy stored fields and the 400ms encoding

Code constants were freely rewritten in milliseconds, since they are not stored anywhere. Admin-set
onchain fields could not be: they hold live values on mainnet, and some are too narrow to hold
milliseconds at all (`liquidation_duration` is a `u8`; 60 seconds is 60,000). Those fields keep
their compact encoding in units of `STORED_UNIT_MS` = 400ms, the historical slot length, and the
encoding is confined to each field's typed getter:

| Stored field | Storage | Getter |
| --- | --- | --- |
| `ValidityGuardRails.slots_before_stale_for_amm` / `_for_margin` | `i64`, 400ms units | `stale_for_amm_ms()` / `stale_for_margin_ms()` -> `Millis` |
| `State.liquidation_duration` | `u8`, 400ms units | `liquidation_duration_ms()` -> `Millis` |
| `State.min_perp_auction_duration` | `u8`, 400ms units | `min_perp_auction_duration_ms()` -> `Millis` |
| `PerpMarket.oracle_slot_delay_override` / `oracle_low_risk_slot_delay_override` | `i8` with sentinels, 400ms units | `DelayOverride::from_immediate` / `from_low_risk` |
| `Constituent.oracle_staleness_threshold` (VLP hedge) | `u64`, 400ms units | decoded inline via `Millis::from_stored_units` |

The `400` is a storage codec detail, the same way nobody "thinks in" `PRICE_PRECISION`: admins
type seconds in the CLI, which encodes on the way in; logic compares `Millis`; the chain stores a
compact integer. New stored durations should store milliseconds natively (a `u32` of ms covers 49
days) and never use this encoding. If a legacy field's encoding ever needs to die, the house
pattern applies: carve a ms-native replacement field from padding, give the getter a fallback,
re-set the value once, and the typed choke point guarantees nothing else in the codebase moves.

A few legacy *rates* were calibrated per-slot in the 400ms era and keep that period explicitly:
the liquidation fee accrues `LIQUIDATION_FEE_INCREASE_PER_PERIOD` per `Millis::UNIT` (400ms) of
elapsed time, the reference-price-offset smoothing budget and the VLP hedge uncertainty-fee bucket
count the same periods, and `get_auction_duration` grants its per-1%-of-price-diff duration in
400ms steps. The period is visible at those sites on purpose: it is part of the tuned economics,
and changing it changes the rate.

## Auctions and the u8 ceiling

Auction durations are the one place a wall-clock value is *stored* in actual slots:
`Order.auction_duration` is a per-order snapshot, minted at placement by
`get_auction_duration` (which computes the intended wall-clock ramp and expresses it via
`to_slots_ceil` at the placement-time slot duration) and then interpolated per-slot as always. An
order's auction is an artifact of the slot regime it was placed in, which is exactly right.

The field is a `u8`, so the inflated duration clamps at 255 actual slots. At 200ms that is ~51
seconds; the historical maximum was 180 slots = ~72 seconds. This is the single deliberate
compression of wall-clock behavior in the design: the extreme tail of auction lengths shortens at
the fastest gates. `BID_ASK_TWAP_MIN_QUOTE_REST` (~9.6s) scales from the same source as
`min_perp_auction_duration` and the `SafeTriggerOrder` horizon, so the OtterSec #146 resting
invariant (a quote must rest at least as long as the auction it can move) survives every gate.

Related non-scaling fact: `Order.posted_slot_tail` is a mod-256 slot stamp (a field width, not a
constant), so the honest order-age window it can express shrinks in wall-clock as slots get
faster (102s at 400ms, 51s at 200ms). The scaled rest requirement (48 actual slots at 200ms)
still fits under it.

## What deliberately does not convert

Chain-slot logic where the slot is the unit of interest, correct at any slot speed:

- same-slot idempotence (`amm.last_update_slot == slot`, per-slot AMM projection, MM-oracle
  monotonicity)
- blockhash validity offsets and landing-slot estimates in the bots
- the `posted_slot_tail` modulus (field width)
- bot liveness tripwires counted in slot ticks (a dead-feed restart firing sooner at faster slots
  is acceptable; documented at each site)

And one legacy oddity kept for compatibility: `block_operation`'s funding gate compares elapsed
400ms periods against `funding_period`, which is denominated in seconds. The pre-existing code
compared a raw slot count against the seconds value (a unit mismatch that silently widened with
faster slots); the gate now holds its current wall-clock width (~40% of the funding period) at
every slot duration.

## Writing new code

The rules reduce to one decision: is the value about wall-clock time, or about slots as slots?

```rust
// a wall-clock rule: write the time you mean
const REBALANCE_COOLDOWN: Millis = Millis::from_secs(30);
if Millis::from_slots(slot - last_rebalance, state.slot_duration()) >= REBALANCE_COOLDOWN { ... }

// genuine slot logic: plain u64, no conversion, no Millis
if amm.last_update_slot == slot { return Ok(()); }   // same-slot idempotence

// a new admin-set duration: store ms natively, skip the legacy encoding entirely
pub cooldown_ms: u32,                                 // in the account
Millis::from_ms(self.cooldown_ms as u64)             // in the getter
```

Getting it wrong does not compile: a raw threshold will not compare against anything the rest of
the system produces (measured deltas arrive as `Millis`, thresholds are consumed as `Millis`),
and the resulting type error leads to `math/time.rs`, whose module doc states this table.

## Off-chain mirrors

Everything off chain reads the same `State.slotDurationMs` field from its existing state
subscription; no service needs a restart at a gate flip.

- **SDK** ([`math/time.ts`](../packages/sdk/src/math/time.ts)): branded `Millis` (a `BN`) and
  `SlotDurationMs` (a `number`), zero runtime cost. `getOracleValidity`, `isOracleValid`,
  `getSpotOracleValidity` and `User.canMakeIdle` take an optional trailing `SlotDurationMs`
  (defaulting to the baseline, so existing callers stay correct until the first gate);
  `calculateMaxPctToLiquidate` takes its ramp as `Millis` (`millisFromStoredUnits(state.liquidationDuration)`).
  `SLOT_TIME_ESTIMATE_MS` is deprecated; there is no correct constant to replace it with, only the
  live field.
- **Bots** (`apps/dlob-server`, `apps/keeper-bots-v2`): all pacing and threshold constants are
  wall-clock ms (`JITO_LEADER_LEAD_MS`, `MARKET_UPDATE_COOLDOWN_MS`, auction duration defaults,
  the vAMM stale-removal threshold), expressed in slots via `msToSlotsNum` and the shared
  `currentSlotDuration(velocityClient)` helper. Operator config knobs are ms
  (`fillAttemptIntervalMs`, `deriskAuctionDurationMs`).
- **Rust bots** (`rust/keep-rs`, `rust/swift`, `rust/velocity-rs`): mirror the program types
  through `velocity_rs::program::math::time`; the swift server's signed-msg staleness gate and
  auction-band staleness knob, keep-rs's oracle-age and liquidation rate limits, and the AMM
  quoting projection all take the live `SlotDuration`.

## Quick reference

| Thing | Value |
| --- | --- |
| Knob | `State.slot_duration_ms` (u16, bytes 1506..1508; `0` = unset = 400ms) |
| Setter | `update_state_slot_duration_ms`, warm admin; allowlist `{350, 300, 250, 200}`, decrease-only |
| CLI | `velocity-admin exchange set-slot-duration-ms <ms>` |
| Duration type | `math::time::Millis` (program), branded `Millis` in `math/time.ts` (SDK) |
| Slot length type | `SlotDuration` / `SlotDurationMs`, sole source `State::slot_duration()` |
| Legacy encoding | `STORED_UNIT_MS` = 400, confined to the stored-field getters in the table above |
| Legacy rate period | `Millis::UNIT` (400ms), explicit at each rate site |
| Ops per gate | one instruction, four gates total |
| Behavior at 400ms | identity: every conversion reproduces the historical slot counts exactly |
| Known compression | max auction length ~51s at 200ms (u8 `Order.auction_duration` ceiling) |
