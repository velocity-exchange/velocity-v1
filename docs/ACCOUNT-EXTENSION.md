# Account extension

How to grow an on-chain zero-copy account such as `User`, `PerpMarket`, `SpotMarket`, or `State`
when its trailing padding runs out. The procedure keeps deployed clients working and leaves
existing accounts usable.

Every zero-copy account in this program is a fixed-size struct. A new field normally goes into
reserved padding, which changes neither the account size nor any offset. Nothing in this document
applies to that case. This document covers the case where a struct must get bigger. That case has
two failure modes, and the protocol handles both.

1. **Old client, new account.** A client compiled against the old struct fetches an account that is
   now longer than it expects. Every decode path in the repo reads exactly the bytes it knows and
   ignores the tail. See [Client behavior](#client-behavior).
2. **New program, old account.** The upgraded program compiles in a bigger `size_of::<T>()`, but
   every existing account still has the old length. The loaders slice `data[8..size_of::<T>() + 8]`,
   which is out of bounds until the account data grows. The `extend_account` migration crank repairs
   this. See [The migration runbook](#the-migration-runbook).

## Why only zero-copy needs this

Borsh decode reads field by field from the front of the buffer, so it ignores trailing bytes for
free. That holds in Rust through `AnchorDeserialize::deserialize` and in TypeScript through
Anchor's coder. Zero-copy decode instead casts a byte range whose length is `size_of::<T>()`. A
reader that derives that range from the *buffer* length, or that demands an exact match, breaks as
soon as the account and the struct disagree.

| Reader                                                                | Behavior on a longer-than-struct account                          |
| --------------------------------------------------------------------- | ------------------------------------------------------------------ |
| Program `AccountLoader` / `load_ref` (on-chain and off-chain replay)   | OK: slices exactly `size_of::<T>()` bytes after the discriminator  |
| Anchor's derived `T::try_deserialize` (zero-copy, off-chain)           | **Panics**: `bytemuck::from_bytes` on the whole tail, exact size    |
| velocity-rs `deser_zero_copy` / `try_deser_zero_copy` / `AccountRef`   | OK: trimmed to `size_of::<T>()` before the cast                     |
| TS SDK: Anchor coder and the custom `decodeUser` fast path             | OK: start-relative offsets only                                     |

The inverse case, an account shorter than the struct, is decodable by nobody. That is the case
`extend_account` repairs.

## What ships in the program

`extend_account` takes `state`, a `payer` signer, an `authority` signer, the target account, and
the system program. The handler reads the account's discriminator, resolves the target size
`8 + size_of::<T>()` for that type from the deployed binary, transfers the rent-exempt shortfall
from the payer, and grows the account data. The runtime zero-fills the new tail. The guard rails
are:

- The `authority` must hold the `AccountExtension` hot role, stored in
  `State.hot_account_extension` and rotated with `update_hot_admin` or
  `velocity-admin auth set-hot-admin accountExtension <pubkey>`. The warm and cold admin also pass,
  and they are the only callers while the role is unset. Extension never corrupts contents, but a
  larger account raises fetch bandwidth and any future per-byte transaction price. The protocol
  therefore decides when accounts grow.
- The account must be velocity-owned and must carry the discriminator of a supported zero-copy
  type. Those are `User`, `UserStats`, `ReferrerName`, `PerpMarket`, `SpotMarket`, `State`,
  `InsuranceFundStake`, `PrelaunchOracle`, `PythLazerOracle`, `RevenueShare`, `LPPool`,
  `Constituent`, `QuoterV0`, `ClobCrankConditionsV0`, `QuoterCrossConditionsV0`, and
  `UserConditionsV0`. Any other discriminator fails with `InvalidAccountExtension`.
- The instruction grows only, and the target size is compiled in. It cannot shrink an account,
  grow one to an arbitrary size, or touch a borsh account.
- An account already at or beyond the target size succeeds and changes nothing. The crank is
  therefore idempotent, a race between crankers is harmless, and one such account does not fail a
  batched transaction.
- The handler takes the account as an `UncheckedAccount` and never loads it as its type. It has to,
  because its purpose is repairing accounts the typed loaders reject.

`extend_account_devnet(new_len)` grows an account to any larger size, under the same role gate. It
lets tests and devnet reproduce the post-upgrade state, where the account is bigger than every
deployed struct, before a real extension exists. Mainnet builds compile it out, which is
`mainnet-beta` without `anchor-test`. Test builds keep it so the integration suite can exercise the
flow.

`extend_account` ships before any real extension, so it is deployed and dormant by the time it is
needed. The upgrade that finally consumes the padding needs no new tooling, because the instruction
resolves its targets from whichever binary is live.

## The migration runbook

The market padding upgrade grows every `PerpMarket` from 1304 to 1560 bytes and every `SpotMarket`
from 808 to 1064 bytes. Both structs append 256 reserved bytes. Deploy the program upgrade, then
run the `perp-market` and `spot-market` extension cranks below at once. The old deployed binary
cannot extend to these sizes ahead of time, because `extend_account` always targets the size
compiled into the live program.

The steps below assume a field no longer fits in `User`'s padding and the struct must grow.

1. **Extend the struct.** Append the fields at the end, or claim trailing padding first. Follow
   [alignment-and-native-offsets.md](./alignment-and-native-offsets.md). Keep `(SIZE - 8) % 16 == 0`
   with explicit tail padding, and keep the u128 and i128 ordering rules. Make sure zeroed bytes are
   a valid default for every new field, because that is what an existing account holds after
   extension. Update `T::SIZE` and the `const_assert_eq!` guards. The init rent math reads the new
   size on its own.
2. **Mirror the layout off-chain.** `bun run program:idl` regenerates the IDL, and the next
   rust-workspace build regenerates `velocity_idl.rs`. Update the hand-maintained
   `packages/sdk/src/types.ts` mirror and the SDK decode paths in the same change, under the rules
   in CLAUDE.md. Add a row to [DRIFT-TO-VELOCITY.md](./DRIFT-TO-VELOCITY.md).
3. **Deploy the upgrade.** From that moment, an instruction that touches an unextended account of
   that type fails with the loader's size error. Nothing is corrupted. The accounts are unreadable
   until they grow. Run the crank right after the deploy.
4. **Crank the extension.** Take the singleton and low-count accounts first, because everything
   loads them, then take the long tail.

   ```bash
   velocity-admin extend-account --type state
   velocity-admin extend-account --type perp-market
   velocity-admin extend-account --type spot-market
   velocity-admin extend-account --type user-stats
   velocity-admin extend-account --type user --batch-size 8
   ```

   The command scans with `getProgramAccounts` by discriminator, skips an account already at size,
   and sends batched `extend_account` instructions. The signing keypair from `--keypair` must hold
   the `AccountExtension` hot role, or be the warm or cold admin. Assign the role before the
   migration with `velocity-admin auth set-hot-admin accountExtension <pubkey>`, so the crank runs
   off a low-value hot key rather than an admin key. `--dry-run` prints the affected accounts and
   the rent cost and sends nothing.

5. **Verify.** Re-run each type with `--dry-run`. Every scan must report zero accounts below
   target. From that point the new fields read as zero defaults until code writes them.

Extend only the types that grew. Extending a type whose struct is unchanged changes nothing, so an
extra crank costs only transaction fees.

## Client behavior

A client must treat the compiled-in struct size as the number of bytes to read, never as the
expected buffer length.

- **TypeScript SDK.** Anchor's borsh coder and the custom `decodeUser` fast path both read
  start-relative offsets, so an extended account decodes unchanged.
  `packages/sdk/tests/decode/extendedAccount.ts` pins this.
- **Rust SDK: velocity-rs, keep-rs, and swift.** Every typed decode goes through
  `utils::deser_zero_copy`, `utils::try_deser_zero_copy`, or `AccountRef`, each of which slices
  `data[8..8 + size_of::<T>()]` before the cast. Do not call anchor's derived `T::try_deserialize`
  on a zero-copy type off-chain. Besides the exact-size panic, it also panics on alignment for a
  16-aligned struct. When a program type is unavoidable, follow the swift `local_sim` pattern: trim
  to `8 + size_of::<T>()`, then copy into `AlignedAccountData`.
- **Never filter by `dataSize`** in `getProgramAccounts`. A hardcoded size matches nothing after an
  extension, and reports no error. Filter by discriminator `memcmp` instead, which every map in the
  repo already does.
- The devnet-gated integration test `tests/velocity/accountExtension.ts` runs the full loop. It
  grows a live `User` and `SpotMarket` past every compiled-in struct, decodes them through both SDK
  paths, and trades against them on chain.

## What this does not cover

- **Borsh accounts** such as `SignedMsgUserOrders`, `RevenueShareEscrow`, and the LP-pool mapping
  accounts. Appending a field to a borsh struct changes the deserialization, not only the buffer
  length. Old bytes then fail to decode as the new type whatever their size. Those accounts version
  their layouts or ship a dedicated resize instruction such as `resize_signed_msg_user_orders`, and
  `extend_account` rejects them.
- **Reordering, inserting, or widening an existing field.** That is a layout break rather than an
  extension, and it reinterprets existing bytes wrongly. The only safe growth claims trailing
  padding and appends past the old end.
- **Shrinking.** It is never supported. It truncates live data, and the rent refund would make any
  shrink path a way to drain funds if its key were ever compromised.
