# Struct alignment, PoolBalance padding, and native instruction offsets

## The problem: u128 alignment diverges between x86_64 and SBF

Rust >= 1.77 corrected `align_of::<u128>()` to 16 bytes on x86_64. The Solana on-chain target
(SBF / BPF) has always kept it at 8 bytes.

For any `#[repr(C)]` struct that contains a `u128` or `i128` field, this means:

- On x86_64 the compiler may insert alignment padding before the field, if it sits at an offset
  that is not a multiple of 16, and tail padding after the last field, to round the total size up
  to the next multiple of 16.
- On SBF neither gap is added, because the alignment requirement is only 8 bytes.

The result is that `std::mem::size_of::<T>()` and field offsets differ between the two platforms
for any struct that contains a u128 and whose total declared content is not already a multiple
of 16.

### Where PoolBalance came in

`PoolBalance` is embedded in `AMM` once (`fee_pool`), in `PerpMarket` twice (`pnl_pool`,
`protocol_fee_pool`) and in `SpotMarket` twice (`revenue_pool`, `protocol_fee_pool`), all as
`#[zero_copy(unsafe)]` structs whose on-chain bytes are the raw memory layout.

Before the fix the struct declared 16 bytes of `u128` plus a `u16` plus 6 bytes of padding, so
x86_64 rounded it up to 32 bytes and SBF left it at 24. Each occurrence therefore contributed
8 bytes of divergence, and the totals compounded with the implicit alignment gaps the same rule
created inside `PerpMarket` and `SpotMarket`. Those pre-fix totals cannot be re-derived from the
current tree; the mechanism above is the part that still matters.

---

## The fix

### 1. Widen PoolBalance padding

`PoolBalance` originally had `padding: [u8; 6]`. It was widened so that the declared content is a
multiple of 16 and `size_of::<PoolBalance>() == 32` on both platforms, with the compiler adding
nothing of its own.

The declared fields have since changed again without moving the size. Today the struct is
`scaled_balance: u128` at 0, `market_index: u16` at 16, `padding: [u8; 2]` at 18,
`pending_interest_split_dust: u32` at 20, and `pending_interest_dust: u64` at 24, for a total of
32 bytes. The two dust fields took 12 of the padding bytes, which is why `padding` is now
`[u8; 2]` rather than the wider array it was introduced with. All four offsets and the size are
pinned in `programs/velocity/src/state/perp_market.rs:1700-1703`.

### 2. Reorder fields in structs that embed PoolBalance

Any struct that placed a `u128`-containing field after a `PoolBalance` field could still develop
an architecture-specific alignment gap between them. Fields were reordered so all `u128` and
`i128` types appear before any `PoolBalance` fields, which removes the gap. `PerpMarket` still
carries that ordering: its eleven `i128`/`u128` counters and `fee_ledger` sit directly after
`pubkey`, ahead of `pnl_pool` and `protocol_fee_pool`.

### 3. Regression guards

Compile time, in the struct's own module. The velocity program uses anonymous `const _: () =
assert!(...)` items, which can appear any number of times in one module and fail the build rather
than a test run:

```rust
// programs/velocity/src/state/perp_market.rs:540
const _: () = assert!(std::mem::size_of::<PerpMarket>() == 1552);
const _: () = assert!(std::mem::offset_of!(PerpMarket, _padding_future) == 1296);

// programs/velocity/src/state/perp_market.rs:1700
const _: () = assert!(std::mem::size_of::<PoolBalance>() == 32);
const _: () = assert!(std::mem::offset_of!(PoolBalance, market_index) == 16);
const _: () = assert!(std::mem::offset_of!(PoolBalance, pending_interest_split_dust) == 20);
const _: () = assert!(std::mem::offset_of!(PoolBalance, pending_interest_dust) == 24);

// programs/velocity/src/state/spot_market.rs:278
const _: () = assert!(std::mem::size_of::<SpotMarket>() == 1056);
const _: () = assert!(std::mem::offset_of!(SpotMarket, _padding_future) == 800);
```

`programs/velocity/src/state/state.rs:648-658` does the same job with
`static_assertions::const_assert_eq!`, which is equivalent for this purpose:

```rust
static_assertions::const_assert_eq!(std::mem::offset_of!(State, slot_duration_ms), 1498);
static_assertions::const_assert_eq!(std::mem::offset_of!(State, pending_slot_duration_ms), 1500);
static_assertions::const_assert_eq!(std::mem::offset_of!(State, slot_duration_effective_slot), 1504);
static_assertions::const_assert_eq!(std::mem::offset_of!(State, slot_duration_transition_slots), 1512);
static_assertions::const_assert_eq!(std::mem::size_of::<State>(), 1744);
```

Place these assertions immediately after the struct definitions they guard. If you change field
types, add fields, or reorder fields and the compiler inserts implicit padding, the build fails.
You do not have to run tests to find out.

`velocity_macros::assert_no_slop` (`crates/velocity-macros/src/lib.rs`) is a third option. It
asserts `size_of::<Struct>() == sum(size_of::<field>())`, which catches implicit padding anywhere
in the struct rather than only a total. It names the constants it emits after the struct
(`<STRUCT>_STRUCT_SIZE`, `<STRUCT>_FIELD_SIZES`), so several uses in one module do not collide,
and it expands to `const_assert_eq!`, so the use site needs `static_assertions::const_assert_eq`
in scope. Only the vaults program currently uses it.

Current sizes, all verified against the assertions and `Size` impls listed in the last column:

| Struct | `size_of` | `T::SIZE` (`size_of` + 8) | Pinned at |
|--------|-----------|---------------------------|-----------|
| `PerpMarket` | 1552 | 1560 | `state/perp_market.rs:540`, `:626` |
| `SpotMarket` | 1056 | 1064 | `state/spot_market.rs:278`, `:363` |
| `State` | 1744 | 1752 | `state/state.rs:658`, `:639` |
| `User` | 4488 | 4496 | `state/user.rs:83` (`Size` impl only) |
| `UserStats` | 232 | 240 | `state/user.rs:2034` (`Size` impl only) |
| `LPPool` | 496 | 504 | `vlp/hedge/state.rs:222` (`Size` impl only) |
| `MarketStats` | 216 | embedded, no discriminator | `state/perp_market.rs:1862` |
| `PoolBalance` | 32 | embedded, no discriminator | `state/perp_market.rs:1700` |
| `HedgeConfig` | 16 | embedded, no discriminator | `vlp/hedge/state.rs:107` |
| `AMM` | 384 | embedded, no discriminator | not pinned anywhere; see below |

`AMM` (`programs/velocity/src/vlp/amm/state.rs`, re-exported from `state::perp_market`) is the one
entry with no assertion of its own. 384 bytes is a measured value from
`std::mem::size_of::<AMM>()` on the host toolchain, and it is consistent with the assertions that
do exist: `PerpMarket` is 1552 bytes, `_padding_future` starts at 1296, `HedgeConfig` occupies the
16 bytes before that, and `amm` occupies the 384 bytes before `hedge_config`. Treat 384 as
correct for the current tree but re-measure it rather than trusting this line after any `AMM`
field change.

Test time, in `programs/velocity/src/state/traits/tests.rs`:

| Module | What it checks |
|--------|----------------|
| `size` | `size_of::<T>() + 8 == T::SIZE` for `OrderActionRecord`, `PerpMarket`, `SpotMarket`, `State`, `User`, `UserStats` and `InsuranceFundStake`, plus three `UserStats` field offsets. If padding diverges between platforms, the hardcoded `SIZE` no longer matches and Anchor account allocation is wrong. |
| `native_instruction_offsets` | The byte offsets the native handlers depend on. See [Native instruction byte offsets](#native-instruction-byte-offsets). |
| `market_index_offset` | Round-trips `PerpMarket` and `SpotMarket` through Anchor's account-info machinery and reads `market_index` at `MARKET_INDEX_OFFSET`. Fails if the byte layout has shifted. |

---

## Padding up vs padding down

### Current reserve audit

All zero-copy accounts must satisfy `(SIZE - 8) % 16 == 0`, which is the same thing as saying
`size_of::<T>()` is a multiple of 16. The gap between the last real field and that total is
reserve space for future fields.

| Struct | Fields end at | `size_of` | Trailing reserve | First 16-aligned offset in the reserve |
|--------|---------------|-----------|------------------|----------------------------------------|
| `PoolBalance` | 32 | 32 | 0 B, `padding: [u8; 2]` is an interior filler, not reserve | none |
| `PerpMarket` | 1296 | 1552 | 256 B (`_padding_future`) | 1296 |
| `SpotMarket` | 800 | 1056 | 256 B (`_padding_future`) | 800 |
| `LPPool` | 314 | 496 | 182 B (`padding`) | 320 |

`PoolBalance` is at its exact minimum. The only room left is the 2-byte interior filler. Anything
larger pushes the struct to 48 bytes and grows every embedding by 16 bytes per occurrence, which
is 32 bytes each for `PerpMarket` and `SpotMarket` and 16 for `AMM`. Treat it as frozen.

### Pad up and pad down

Pad up means increasing the total `SIZE` to the next higher multiple of 16. Pad down means
reducing it to the next lower multiple of 16.

For live mainnet accounts neither is free: `SIZE` is fixed by what was already allocated on-chain
when the accounts were created. Changing `SIZE` requires the `extend_account` migration crank and
the runbook in [ACCOUNT-EXTENSION.md](./ACCOUNT-EXTENSION.md). Growing is supported; shrinking is
not, because it truncates live data. So for any deployed account type, pad up is the only
direction available, and pad down applies only to a new struct that has never been allocated.

In both cases the rule is the same: `(SIZE - 8)` must be a multiple of 16.

Why 16 and not 8: the target that matters most is the deployed SBF VM, where u128 alignment is 8,
but the tests, the SDK and every local build run on x86_64, where it is 16. A multiple of 16
satisfies both at once and keeps `size_of` identical on all platforms, which is a hard
requirement for zero-copy accounts.

---

## Managing field changes in zero-copy structs

Zero-copy (`#[account(zero_copy)]`) structs have a fixed on-chain layout. Any change that shifts a
field's byte position is a breaking change, because existing accounts hold data at the old
offsets.

### The invariants you must preserve

1. `(SIZE - 8) % 16 == 0`, so the declared content is a multiple of 16.
2. No `u128` or `i128` field appears after a `PoolBalance` field. That ordering re-introduces an
   implicit 8-byte alignment gap on x86_64.
3. Every `u128` and `i128` field starts at an offset that is a multiple of 16. Otherwise x86_64
   inserts an internal alignment gap that SBF does not, and `size_of` diverges again.
4. Every gap is declared. If the compiler would insert padding, write it as an explicit
   `padding_*: [u8; N]` field so the IDL records it and off-chain borsh decoders read the same
   offsets the program does.

### Adding a new field

1. Take the bytes out of the trailing reserve. Shrink `_padding_future` (or `padding`) by the size
   of the new field and declare the new field immediately before it. The total stays constant, so
   the multiple-of-16 invariant holds without any further arithmetic and no account has to be
   reallocated.
2. Check the offset the field lands at against its own alignment. A `u64` needs an offset that is
   a multiple of 8, a `u128` or `i128` a multiple of 16. If the reserve starts at an offset that
   does not satisfy this, add an explicit filler field ahead of the new field and take those bytes
   from the reserve as well. Never let the compiler insert the gap for you.
3. Respect the ordering rule: `u128` and `i128` fields must precede `PoolBalance` fields.
4. Make sure all-zero bytes are a valid default, because that is what every existing account holds
   in the reserve.
5. If the reserve is too small for the field, the struct has to grow. Round the new `size_of` up
   to a multiple of 16, update `T::SIZE` and the compile-time assertions, and run the
   `extend_account` migration in [ACCOUNT-EXTENSION.md](./ACCOUNT-EXTENSION.md) immediately after
   the program upgrade.
6. Run the size regression tests: `cargo test -p velocity size`.

### Removing a field (replacing with padding)

1. Replace the field with an equivalently sized `padding_*: [u8; N]` array so every following field
   keeps its offset.
2. `(SIZE - 8)` does not change, so the multiple-of-16 invariant is preserved automatically.
3. Do not shrink the struct. On-chain accounts are already allocated at the current `SIZE`, and
   there is no shrink path.

### Growing a field (for example u32 to u64)

This is the same as removing the old field and adding a larger one in place. Consume the extra 4
bytes from the reserve and re-check the alignment of every field that follows, since the one you
widened may now push a `u64` or `u128` off its required offset.

### Example: adding a u64 to PerpMarket

`PerpMarket` fields end at 1296 and `_padding_future` is `[u8; 256]`, for a total of 1552.

```
before: fields end at 1296, reserve 256, size_of 1552
add u64: field at 1296 (1296 % 8 == 0), reserve becomes [u8; 248]
after:  fields end at 1304, reserve 248, size_of 1552
```

1552 is unchanged and still a multiple of 16. 248 bytes of reserve remain.

### Example: adding a u128 to PerpMarket

```
before: fields end at 1296, reserve 256, size_of 1552
add u128: field at 1296 (1296 % 16 == 0), reserve becomes [u8; 240]
after:  fields end at 1312, reserve 240, size_of 1552
```

1296 is a multiple of 16, so the `u128` needs no filler ahead of it and both targets agree.

### Example: a u128 at an offset that is not a multiple of 16

Take the `u64` change above and add a `u128` on top of it. The reserve now starts at 1304, which
is a multiple of 8 but not of 16. Declaring the `u128` there and shrinking the reserve to
`[u8; 232]` gives declared content of 1304 + 16 + 232 = 1552, which looks correct, but the two
targets disagree:

```
x86_64: compiler inserts 8 bytes before the field; it starts at 1312; size_of 1560
SBF:    no gap; the field starts at 1304; size_of 1552
```

The compile-time assertion fires on the host build, which is the point of it. The fix is an
explicit filler:

```
padding_align_new: [u8; 8]   // 1304..1312, taken from the reserve
new_field: u128              // 1312, a multiple of 16
_padding_future: [u8; 224]   // 1312 + 16 + 224 == 1552
```

Both targets now put the field at 1312 and `size_of` is 1552 everywhere.

### Example: adding a u64 to SpotMarket

`SpotMarket` fields end at 800 and `_padding_future` is `[u8; 256]`, for a total of 1056.

```
before: fields end at 800, reserve 256, size_of 1056
add u64: field at 800 (800 % 8 == 0), reserve becomes [u8; 248]
after:  fields end at 808, reserve 248, size_of 1056
```

800 is also a multiple of 16, so a `u128` would fit at the same offset with no filler.

### Example: running out of reserve

`PerpMarket` has 256 bytes of reserve. The field that does not fit is the one that grows the
account. Round the new total up to a multiple of 16, update `PerpMarket::SIZE` and the
`const _: () = assert!` guards, and plan the upgrade as a migration: deploy, then crank
`velocity-admin extend-account --type perp-market` before the market accounts are loaded again.
`PerpMarket` and `SpotMarket` have already been through this once, growing from 1304 to 1560 and
from 808 to 1064 bytes respectively when the 256-byte reserves were appended.

---

## Native instruction byte offsets

Three instruction handlers bypass Anchor's deserializer entirely and write directly into raw
account bytes at hardcoded offsets. The custom entrypoint dispatches them on the discriminator
`[0xFF, 0xFF, 0xFF, 0xFF, opcode]` (`programs/velocity/src/lib.rs:58`):

- `handle_update_mm_oracle_native` (opcode 0, `instructions/admin.rs:3911`) writes
  `mm_oracle_slot`, `mm_oracle_price` and `mm_oracle_sequence_id` into a `PerpMarket` account, and
  reads `feature_bit_flags`, `hot_mm_oracle_crank` and the slot-duration clock from a `State`
  account.
- `handle_update_amm_spread_adjustment_native` (opcode 1, `vlp/amm/admin.rs:1336`) writes
  `amm_spread_adjustment` into a `PerpMarket` account and reads `hot_amm_spread_adjust` from a
  `State` account.
- `handle_update_mm_oracle_batch_native` (opcode 2, `instructions/admin.rs:4192`) makes the same
  writes as opcode 0, for up to `MM_ORACLE_BATCH_MAX_MARKETS` (64) `PerpMarket` accounts in one
  instruction.

Opcodes 0 and 2 read the `State` fields through the shared `STATE_FEATURE_BIT_FLAGS_OFFSET`,
`STATE_HOT_MM_ORACLE_CRANK_OFFSET` and `STATE_SLOT_DURATION_*_OFFSET` constants in
`instructions/admin.rs:3850-3869`, and opcode 1 through `STATE_HOT_AMM_SPREAD_ADJUST_OFFSET` in
`vlp/amm/admin.rs:1334`, so the handlers cannot desync from each other. The constants still have
to be kept in step with the layout, which is what the offset tests below are for.

### How to compute an offset

`PerpMarket`, `AMM` and `State` are all zero-copy (`#[account(zero_copy(unsafe))]` plus
`#[repr(C)]`), so the on-chain bytes are the memory layout and the account offset of a field is
`std::mem::offset_of!(Struct, field) + 8`, where 8 is the Anchor discriminator. `State` used to be
a regular borsh `#[account]`, which needed a borsh round-trip to locate a field; it is zero-copy
now, so `offset_of!` is correct for every account listed here.

### Current hardcoded offsets

The MM-oracle fields live on `PerpMarket::market_stats` (`MarketStats`), not on `AMM`. They were
moved there in the AMM-decoupling refactor.

| Field | Account | Offset | How derived |
|-------|---------|--------|-------------|
| `MarketStats::mm_oracle_price` | `PerpMarket` | 800 | `offset_of!(PerpMarket, market_stats) + offset_of!(MarketStats, mm_oracle_price) + 8` |
| `MarketStats::mm_oracle_slot` | `PerpMarket` | 808 | same |
| `MarketStats::mm_oracle_sequence_id` | `PerpMarket` | 816 | same |
| `AMM::amm_spread_adjustment` | `PerpMarket` | 1282 | `offset_of!(PerpMarket, amm) + offset_of!(AMM, amm_spread_adjustment) + 8` |
| `State::hot_mm_oracle_crank` | `State` | 360..392 | `offset_of!(State, hot_mm_oracle_crank) + 8` |
| `State::hot_amm_spread_adjust` | `State` | 392..424 | same |
| `State::feature_bit_flags` | `State` | 1374 | same |
| `State::slot_duration_ms` | `State` | 1506..1508 | same |
| `State::pending_slot_duration_ms` | `State` | 1508..1510 | same |
| `State::slot_duration_effective_slot` | `State` | 1512..1520 | same |
| `State::slot_duration_transition_slots` | `State` | 1520..1552 | same, `[u64; 4]` |
| `State::hot_vamm_quote_management` | `State` | 1552..1584 | same, allocated from former padding |

Only the `State` offsets are read by raw index. The `PerpMarket` fields are reached through a
`bytemuck` cast and typed field access, and are asserted here purely as layout invariants.

### Regression tests

`programs/velocity/src/state/traits/tests.rs :: native_instruction_offsets` locks these values
down:

- `amm_zero_copy_offsets` asserts the three `MarketStats` MM-oracle offsets and
  `AMM::amm_spread_adjustment`. It also asserts that `PerpMarket::fee_ledger` is 16-aligned, that
  `bankruptcy_if_floor_pct` occupies the 4 bytes immediately before `market_stats`, and that
  `pending_bankruptcy_claims` sits 2-aligned in the 6 bytes before `last_fill_price`. Those three
  fields were carved out of former padding, so they have to stay exactly where they are.
- `state_feature_bit_flags_offset`, `state_hot_mm_oracle_crank_offset` and
  `state_hot_amm_spread_adjust_offset` assert the `State` offsets the handlers index directly.
- `state_slot_duration_offsets` asserts the three slot-duration offsets
  `read_native_state_slot_clock` reads.
- `state_hot_vamm_quote_management_offset` asserts that the quote-management authority stays in
  former padding.

If you change any field in `AMM`, `PerpMarket` or `State`, run
`cargo test -p velocity native_instruction_offsets` and update the test expectations and the
offset constants used by `handle_update_mm_oracle_native`,
`handle_update_mm_oracle_batch_native` and `handle_update_amm_spread_adjustment_native` together.

---

## SDK custom user decoder (`packages/sdk/src/decode/user.ts`)

The SDK ships a hand-written binary decoder for `UserAccount` that reads the raw on-chain bytes
directly rather than going through Anchor's borsh coder. It is tested in
`packages/sdk/tests/decode/test.ts`, which decodes 100 real mainnet buffers and asserts
field-by-field equality against Anchor's own decoder.

It exists because Anchor's borsh coder allocates many intermediate objects. The custom decoder
produces roughly 15x smaller output and is measurably faster on high-frequency subscription paths.

### How the decoder works

The decoder keeps a running `offset` counter starting at 8, past the discriminator, and reads each
field in declaration order. That is the same order zero-copy structs use in memory and the same
order borsh serialises in. For `PerpPosition` and `SpotPosition` it also pre-reads a few fields at
fixed relative offsets before the loop body, to decide whether to skip the slot entirely.

### What must be updated when UserAccount or its sub-structs change

`UserAccount` and its embedded types (`SpotPosition`, `PerpPosition`, `Order`) are not zero-copy
on-chain. They are regular `#[account]` structs serialised with borsh. But because they contain
only fixed-size primitive fields, with no `Vec`, `String` or `Option`, the borsh wire format is
identical to a packed `repr(C)` layout: fields are written sequentially with no gaps.

| Change | What to update in `decode/user.ts` |
|--------|------------------------------------|
| Field added to `PerpPosition` / `SpotPosition` / `Order` | Add a read at the correct offset, update the `offset +=` arithmetic for all subsequent fields, add the field to the returned object literal |
| Field removed (replaced with padding) | Replace the read with `offset += N`, remove it from the returned object literal, remove it from the `PerpPosition` / `SpotPosition` TypeScript type in `packages/sdk/src/types.ts` and any default-value sites |
| Field type widened (for example `u32` to `u64`) | Change the reader and update the `offset +=` delta |
| Field reordered | Reorder the reads to match; every subsequent absolute pre-read (`offset + N`) must be recalculated |

Signed versus unsigned: use `readSignedBigInt64LE` for `i64` fields and `readUnsignedBigInt64LE`
for `u64` fields. The two diverge only when bit 63 is set, and a mismatch causes a silent `.eq()`
failure in the decode test rather than a crash, so it is easy to miss without running the test.

Padding: when a field is removed from the Rust struct and replaced with `padding_x: [u8; N]`, the
decoder must skip those bytes with `offset += N` rather than dropping the read. Otherwise every
field that follows shifts by N bytes and all subsequent assertions fail.

### Running the test

```bash
cd packages/sdk
bun run test --grep "Custom user decode"
```

The test decodes 100 real mainnet `UserAccount` buffers and compares every field between the
custom decoder and Anchor's borsh decoder. Run it any time `UserAccount`, `PerpPosition`,
`SpotPosition` or `Order` changes layout.

---

## Quick reference: what breaks if you get this wrong

| Mistake | Symptom |
|---------|---------|
| PoolBalance padding too small | `size_of::<PerpMarket>()` differs between dev (x86\_64) and on-chain (SBF); `size` tests fail on one platform |
| u128 field placed after PoolBalance | Alignment gap re-introduced; size diverges silently at runtime |
| New field pushes the total off a multiple of 16 | x86\_64 adds implicit tail padding; `size_of` diverges; `size` tests fail |
| New u128 placed at an offset that is not a multiple of 16 | x86\_64 inserts an internal gap SBF does not; the compile-time assertion fires on the host build |
| `(SIZE - 8)` changed without running the `extend_account` crank | The loaders slice past the end of every existing account; instructions touching them fail until they are grown |
| Using `offset_of!` for a borsh account | Native handler reads or writes the wrong field; an assertion fires or data is silently corrupted |
| Updating AMM/PerpMarket/State layout without updating the offset constants | Native instructions write to stale offsets; values appear unchanged after the transaction |
| Changing `PerpPosition` / `SpotPosition` / `Order` layout without updating `packages/sdk/src/decode/user.ts` | Custom decoder reads the wrong bytes; the `Custom user decode` SDK test fails with field-value mismatches |
| Using `readSignedBigInt64LE` for a `u64` field in `decode/user.ts` | Silent mismatch against Anchor when bit 63 is set; `.eq()` in the decode test fails |
