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

The live slot length is held by three `State` fields carved in place from the padding after
`promo_fee_tier` (offsets pinned by `const_assert_eq!` and the native-offset guard test):

- `slot_duration_ms` (`u16`, bytes 1506..1508): the current base value. Pre-upgrade accounts read
  `0` out of former padding, which means "unset" and resolves to the 400ms baseline, so there was
  no migration.
- `pending_slot_duration_ms` (`u16`, bytes 1508..1510): a staged next value, `0` when nothing is
  staged.
- `slot_duration_effective_slot` (`u64`, bytes 1512..1520): the slot at which the staged value takes
  effect.

Never read the fields directly: `State::slot_duration()` reads the current slot from the Clock
sysvar and returns the staged value once `slot_duration_effective_slot` has been reached, otherwise
the base — so **State switches itself at the boundary with no second transaction**. The switch
decision lives in one place, `math::time::active_slot_duration_ms`, shared by `slot_duration()`, the
native fast-path reader, and the off-chain mirrors.

The setter is `update_state_slot_duration_ms` (warm admin; CLI:
`velocity-admin exchange set-slot-duration-ms <ms>`). It *stages* the switch during the target
gate's warmup:

1. **Exact successor only.** The value must be the one immediately after the current effective
   duration on the schedule `400 -> 350 -> 300 -> 250 -> 200` (`next_slot_duration_ms`):
   monotonic-decreasing (slots never get slower again; feature gates cannot deactivate), skips
   rejected (so any in-flight measurement crosses at most one step), non-schedule typos rejected —
   all in one check. Staging the next value first promotes an already-effective pending into the
   base.
2. **Effective slot read from the gate.** The instruction takes the target's IBRL feature-gate
   account as a remaining account, verifies it (owned by `Feature111…`, activated with `data[0] == 1`),
   and reads its activation slot; `slot_duration_effective_slot = activation + one epoch (432,000
   slots)`. It does **not** require the warmup to have elapsed — the activation slot is exposed one
   epoch ahead precisely so State can be staged during the warmup and flip in lockstep with the
   chain. The SDK/CLI fill in the account from the target value.

**Why lockstep matters — there is no universally-safe direction for a mismatch** between the live
`slot_duration_ms` and the real slot length. A conversion serves both risk *ceilings* (oracle
staleness: a shorter window is safer) and user-protection *minima* (liquidation ramps, grace
periods: a longer window is safer), and the two want opposite errors. With max allowed real oracle
age `= (window_ms / live_ms) x real_slot_ms`:

- **live value ahead of the chain** (200ms while the chain is still 400ms): a 48s oracle window
  becomes `48000/200 x 400 = 96s` — it *widens*, accepting staler oracles (unsafe ceiling); ramps
  meanwhile *lengthen* (safe for the user).
- **live value behind the chain** (400ms while the chain is already 200ms): the oracle window
  becomes `48000/400 x 200 = 24s` — it *tightens* (safe ceiling, at a liveness cost); but ramps and
  grace periods *shorten* to half their intended wall-clock (unsafe minimum).

Staging during the warmup makes the effective value match the chain at the exact boundary, so
neither lag is ever live in steady state — which is the whole point of reading the effective slot
from the gate rather than requiring a hand-timed transaction at the boundary.

Runbook, in full:

```
# during the target gate's warmup epoch (feature activated, not yet effective),
# once the Feature Gate Tracker shows it activated for the *next* epoch:
velocity-admin exchange set-slot-duration-ms 350   # then 300, 250, 200 as each is activated

# once the 200ms transition is effective, finalize the raw base field
velocity-admin exchange set-slot-duration-ms 200
```

Each transition itself still needs only its one staging transaction: State switches automatically
at the effective slot. The final 200ms command is a bookkeeping transaction after the last switch;
it promotes the already-effective pending value into `slot_duration_ms` because there is no later
gate whose staging transaction could perform that promotion. It does not control or delay the live
switch. No guard-rail retunes, per-market updates, or bot restarts are needed (the off-chain mirrors
read the same fields from their state subscription and apply the switch against a live chain slot).

### The one residual: measurements that straddle the switch

Staging removes the operator-timing lag, but it does not remove the arithmetic residual at the
switch instant. The conversions assume every slot in a measured delta had the current duration;
Solana's slot *counter* does not record that earlier slots were longer, so a measurement whose
interval began before the switch slot and ends after it is converted with the new (shorter) duration
and reads marginally younger than its true wall-clock age. That under-count is safe for elapsed/ramp
sites (favors the user) and unsafe for oracle-freshness gates (briefly accepts a slightly-staler
oracle).

Two honest caveats on the magnitude:

- A warm admin catching a lagged State up can stage successive gates back-to-back, so a measurement
  straddling that catch-up can see more than one step at once. Each step still requires its gate to
  be staged, so it only happens when the chain has genuinely passed those gates.
- A measurement window longer than the gap between two real gate activations naturally spans more
  than one transition. In practice the risk-sensitive windows (oracle staleness ~seconds,
  liquidation ramp ~minutes) are far shorter than the weeks-apart rollout, so they span at most one
  transition; only the long user-protection windows (idle, force-delete) can span more, and their
  under-count direction favors the user.

We accept this residual rather than carry a full per-regime activation-slot clock. The robust
alternative, if the tail ever bites, is to keep the history of effective transition slots in State
and compute elapsed time piecewise (`slot_duration_at(slot)` + a summed walk over the transitions).

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

**`StoredSlotDuration<T, const SLOT_MS: u64>`** is the compact account-storage type. `T` fixes the
wire width and `SLOT_MS` records the slot length assumed when that field was created. Thus
`StoredSlotDuration<u8, 400>` still occupies one byte, but a raw `10` unambiguously means 4,000ms;
`StoredSlotDuration<u8, 200>` with the same raw byte means 2,000ms. Program logic calls
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
JavaScript coder does not yet flatten transparent generic wrappers; SDK users therefore keep the
existing `u8`/`i64`/`u64` decoded shapes. Signed fields with sentinel values are intentionally not
modeled as ordinary durations and continue through `DelayOverride`.

Changing a field from (for example) `StoredSlotDuration<u8, 400>` to
`StoredSlotDuration<u8, 200>` changes the interpretation of every existing byte. That is a real
data migration: coordinate the program upgrade with an admin rewrite that preserves each field's
wall-clock value. Never change only the const parameter.

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
| `ValidityGuardRails.slots_before_stale_for_amm` / `_for_margin` | `StoredSlotDuration<i64, 400>` (IDL: `i64`) | `stale_for_amm_ms()` / `stale_for_margin_ms()` -> `Millis` |
| `State.liquidation_duration` | `StoredSlotDuration<u8, 400>` (IDL: `u8`) | `liquidation_duration_ms()` -> `Millis` |
| `State.min_perp_auction_duration` | `StoredSlotDuration<u8, 400>` (IDL: `u8`) | `min_perp_auction_duration_ms()` -> `Millis` |
| `PerpMarket.oracle_slot_delay_override` / `oracle_low_risk_slot_delay_override` | `i8` with sentinels, 400ms units | `DelayOverride::from_immediate` / `from_low_risk` |
| `Constituent.oracle_staleness_threshold` (VLP hedge) | `StoredSlotDuration<u64, 400>` (IDL: `u64`) | normalized to `Millis` at use |

The `400` is a storage codec detail, the same way nobody "thinks in" `PRICE_PRECISION`: admins
type seconds in the CLI, which encodes on the way in; logic compares `Millis`; the chain stores a
compact integer. New compact stored durations should encode their chosen quantum in
`StoredSlotDuration<T, SLOT_MS>`; use a native millisecond integer instead when arbitrary
millisecond precision matters more than width. If a legacy field's encoding ever needs to die, the house
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

### Paths that run on the baseline (no `State` in scope)

A few instruction paths load oracle validity without a velocity `State` account in scope and so
decode the guard-rail staleness windows at the 400ms baseline regardless of the live slot duration:
`jit-proxy`'s `check_order_constraints`, the two `UpdateUser` handlers (margin-trading toggle,
pool-id), and the vaults `manager_update_borrow` instruction (its account struct carries no velocity
`State`). Every other vaults instruction now resolves the live duration from its `velocity_state`
account via `State::slot_duration_from_account_info` (owner + discriminator validated, staging fields
read by offset), so only that one vault path stays on the baseline. Floor-tightening is the safe
direction (a 4s window becomes 2s at 200ms, stricter, never more permissive), so the remaining
baseline paths are a liveness note, not a safety gap: at 200ms they want an oracle cranked within
~2s. Threading `State` into `manager_update_borrow` (an account/ABI change) is the one step left if
that tightening ever bites. keep-rs's liquidator and filler re-sample the slot duration on a live
cadence — the liquidator's rate limiter reads a shared value the main loop refreshes, the filler
refreshes on an elapsed-slot config tick — so a process spanning a gate activation picks up the new
duration without a restart.

## Writing new code

The rules reduce to one decision: is the value about wall-clock time, or about slots as slots?

```rust
// a wall-clock rule: write the time you mean
const REBALANCE_COOLDOWN: Millis = Millis::from_secs(30);
if Millis::from_slots(slot - last_rebalance, state.slot_duration()) >= REBALANCE_COOLDOWN { ... }

// genuine slot logic: plain u64, no conversion, no Millis
if amm.last_update_slot == slot { return Ok(()); }   // same-slot idempotence

// a compact admin-set duration: the field type records its storage quantum
pub cooldown: StoredSlotDuration<u16, 200>,
let cooldown = self.cooldown.to_millis();

// if arbitrary millisecond precision matters more than compactness, store ms directly
pub cooldown_ms: u32,
let cooldown = Millis::from_ms(self.cooldown_ms as u64);
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
  live field. `currentSlotDuration` / `currentSlotClock` resolve that field for any client holding a
  subscribed `State` (see [Off-chain clients](#off-chain-clients-sdk-common-ts-ui-bots)).
- **Bots** (`apps/dlob-server`, `apps/keeper-bots-v2`): all pacing and threshold constants are
  wall-clock ms (`JITO_LEADER_LEAD_MS`, `MARKET_UPDATE_COOLDOWN_MS`, auction duration defaults,
  the vAMM stale-removal threshold), expressed in slots via `msToSlotsNum` and the SDK's
  `currentSlotDuration(client, currentSlot)` helper (pass a live chain slot so a staged
  switch is applied; unavailable state/slot falls back to the 400ms baseline). Operator config
  knobs are ms (`fillAttemptIntervalMs`, `deriskAuctionDurationMs`).
- **Rust bots** (`rust/keep-rs`, `rust/swift`, `rust/velocity-rs`): mirror the program types
  through `velocity_rs::program::math::time`; the swift server's signed-msg staleness gate and
  auction-band staleness knob, keep-rs's oracle-age and liquidation rate limits, and the AMM
  quoting projection all take the live `SlotDuration`.

### Off-chain clients (SDK, common-ts, UI, bots)

Every TypeScript client converts through the same live field, and **no client holds a slot
length**: not a constant, not a config value, not a default parameter. `SLOT_TIME_ESTIMATE_MS`
stays exported and deprecated for one minor series only so consumers can bump without a flag day;
there is no correct constant to replace it with.

The single entry point is `currentSlotDuration(source, currentSlot)` in
[`math/time.ts`](../packages/sdk/src/math/time.ts) (`currentSlotClock` returns the same value plus
an `isLive` flag). `source` is duck-typed on `{ getStateAccount() }`, so anything holding a
subscribed `State` works, and `math/time` stays free of client imports. Two rules the resolver
enforces so callers cannot get them wrong:

- **`currentSlot` must be the live chain slot**, not the slot `State` was last written at. `State`
  does not change at the gate boundary, so a cached State slot would never trigger the staged
  switch.
- **A missing or `0` slot is a dead feed, not slot zero.** A failed slot subscription reports `0`,
  and slot `0` precedes every effective slot, so treating it as live would return the pre-flip base
  while looking correct. The resolver returns the hardcoded 400ms `SLOT_DURATION_BASELINE` and
  `isLive: false` instead.

**The fallback is the baseline, not per call site.** Unavailable state/slot falls back to 400ms
(the longest scheduled slot) so risk ceilings tighten. Callers never pass a fallback; user-
protection windows that need a shorter under-promise while the feed is down should not rely on
this helper's dead-feed path.

**Program mirrors convert exactly as the program does.** Where a client reproduces an on-chain
computation (auction durations above all), mirror the program's arithmetic step for step: the same
`Millis` intent, the same clamps, the same rounding direction, and the same `min(255)` cap that
`Order.auction_duration`'s `u8` imposes. The reference implementation is the auction-param builder
in `apps/dlob-server/src/utils/utils.ts`. A mirror that floors where the program ceils, or that
skips the cap, makes the client predict a different fill than the chain grants.

## Quick reference

| Thing | Value |
| --- | --- |
| Knob | `State.slot_duration_ms` (u16, bytes 1506..1508; `0` = unset = 400ms) |
| Setter | `update_state_slot_duration_ms`, warm admin; allowlist `{350, 300, 250, 200}`, decrease-only |
| CLI | `velocity-admin exchange set-slot-duration-ms <ms>` |
| Duration types | `Millis` for arithmetic; `StoredSlotDuration<T, SLOT_MS>` for compact account storage |
| Slot length type | `SlotDuration` / `SlotDurationMs`, sole source `State::slot_duration()` |
| Legacy encoding | `STORED_UNIT_MS` = 400, carried in the stored fields' Rust types and normalized to `Millis` |
| Legacy rate period | `Millis::UNIT` (400ms), explicit at each rate site |
| Ops | one staging instruction per gate, plus one post-200ms base-field finalization |
| Behavior at 400ms | identity: every conversion reproduces the historical slot counts exactly |
| Known compression | max auction length ~51s at 200ms (u8 `Order.auction_duration` ceiling) |
