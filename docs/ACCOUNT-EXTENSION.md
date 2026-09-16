# Account extension

How to grow an on-chain zero-copy account (`User`, `PerpMarket`, `SpotMarket`, `State`, ...) when
its trailing padding runs out, without breaking deployed clients or bricking existing accounts.

Every zero-copy account in this program is a fixed-size struct. New fields normally go into
reserved padding, which changes neither the account size nor any offset, and nothing in this doc
applies. This doc is for the day a struct genuinely needs to get **bigger**. That day has two
failure modes, and the protocol now handles both:

1. **Old client, new account.** A client compiled against the old struct fetches an account that
   is now longer than it expects. Handled everywhere: all our decode paths read exactly the bytes
   they know and ignore the tail (see [Client behavior](#client-behavior)).
2. **New program, old account.** The upgraded program compiles in a bigger `size_of::<T>()`, but
   every existing account still has the old length. The loaders slice
   `data[8..size_of::<T>() + 8]`, which is out of bounds until the account data is physically
   grown. Handled by the `extend_account` migration crank (see [The migration
   runbook](#the-migration-runbook)).

## Why only zero-copy needs this

Borsh decode is field-by-field from the front of the buffer, so trailing bytes are ignored for
free, in Rust (`AnchorDeserialize::deserialize`) and in TypeScript (Anchor's coder). Zero-copy
decode instead casts a byte range whose length is `size_of::<T>()`; anything that derives that
range from the *buffer* length, or demands an exact match, breaks the moment the account and the
struct disagree:

| Reader                                                                | Behavior on a longer-than-struct account                          |
| --------------------------------------------------------------------- | ------------------------------------------------------------------ |
| Program `AccountLoader` / `load_ref` (on-chain and off-chain replay)   | OK: slices exactly `size_of::<T>()` bytes after the discriminator  |
| Anchor's derived `T::try_deserialize` (zero-copy, off-chain)           | **Panics**: `bytemuck::from_bytes` on the whole tail, exact size    |
| velocity-rs `deser_zero_copy` / `try_deser_zero_copy` / `AccountRef`   | OK: trimmed to `size_of::<T>()` (fixed in the dynamic-size-clients PR) |
| TS SDK: Anchor coder and the custom `decodeUser` fast path             | OK: start-relative offsets only                                     |

The inverse case (account shorter than the struct) is not decodable by anyone and is exactly what
`extend_account` exists to repair.

## What ships in the program

**`extend_account`**. Takes `state`, `payer` (signer), an `authority` signer holding the
`AccountExtension` hot role (warm/cold admin pass too, and are the only callers while the role is
unset), the target account, and the system program. The handler reads the account's
discriminator, resolves the target size `8 + size_of::<T>()` for that type from the deployed
binary, transfers the rent-exempt shortfall from the payer, and grows the account data; the
runtime zero-fills the new tail. Guard rails:

- Auth: `HotRole.AccountExtension` (`State.hot_account_extension`, rotated with
  `update_hot_admin` / `velocity-admin auth set-hot-admin accountExtension <pubkey>`). Extension
  never corrupts contents, but growing accounts inflates fetch bandwidth and any future per-byte
  transaction pricing, so when accounts grow is the protocol's decision, not the public's.
- The account must be velocity-owned and carry the discriminator of a supported zero-copy type
  (`User`, `UserStats`, `ReferrerName`, `PerpMarket`, `SpotMarket`, `State`,
  `InsuranceFundStake`, `PrelaunchOracle`, `PythLazerOracle`, `RevenueShare`, `LPPool`,
  `Constituent`). Anything else fails with `InvalidAccountExtension`.
- Grow-only, and the target is compiled in, so the instruction cannot shrink an account, inflate
  one to an arbitrary size, or touch borsh accounts.
- An account already at (or beyond) target size is a success no-op, so the crank is idempotent,
  races between crankers are harmless, and batched transactions never fail wholesale.
- The handler takes the account as an `UncheckedAccount` and never loads it as its type. This
  matters: the whole point is repairing accounts the typed loaders currently reject.

**`extend_account_devnet(new_len)`** grows an account to an arbitrary larger size, under the same
role gate. It exists so tests and devnet can simulate the post-upgrade state (account bigger than
every deployed struct) before a real extension exists. Compiled out of production mainnet builds
(`mainnet-beta` without `anchor-test`); test builds keep it so the integration suite can exercise
the flow.

Because `extend_account` ships before any real extension, it is already deployed and dormant by
the time it is needed. It resolves its targets from whatever binary is live, so the upgrade that
consumes the last of a struct's padding needs no new instruction.

## The migration runbook

The market padding upgrade grows every `PerpMarket` from 1304 to 1560 bytes and every
`SpotMarket` from 808 to 1064 bytes. Both accounts append 256 reserved bytes. Deploy the program
upgrade, then immediately run the `perp-market` and `spot-market` extension cranks below. The old
deployed binary cannot pre-extend to these sizes because `extend_account` always targets the size
compiled into the live program.

Say a field no longer fits in `User`'s padding and the struct must grow.

1. **Extend the struct.** Append fields at the end (or claim trailing padding first). Follow
   [alignment-and-native-offsets.md](./alignment-and-native-offsets.md): keep
   `(SIZE - 8) % 16 == 0` with explicit tail padding, keep u128/i128 ordering rules, and make sure
   zeroed bytes are a valid default for every new field, because that is what existing accounts
   will hold after extension. Update `T::SIZE`, the `const_assert_eq!` guards, and the init rent
   math picks the new size up automatically.
2. **Mirror the layout off-chain.** `bun run program:idl` regenerates the IDL (and
   `velocity_idl.rs` on the next rust-workspace build); update the hand-maintained
   `packages/sdk/src/types.ts` mirror and the SDK decode paths in the same change, per the usual
   rules in CLAUDE.md. Add a row to [DRIFT-TO-VELOCITY.md](./DRIFT-TO-VELOCITY.md).
3. **Deploy the upgrade.** From this moment, instructions touching a not-yet-extended account of
   that type fail with the loader's out-of-bounds/size error. Nothing is corrupted; the accounts
   are just unreadable until grown. Plan the crank to follow immediately.
4. **Crank the extension.** Singleton and low-count accounts first, since everything loads them,
   then the long tail:

   ```bash
   velocity-admin extend-account --type state
   velocity-admin extend-account --type perp-market
   velocity-admin extend-account --type spot-market
   velocity-admin extend-account --type user-stats
   velocity-admin extend-account --type user --batch-size 8
   ```

   The command scans with `getProgramAccounts` by discriminator, skips accounts already at size,
   and sends batched `extend_account` instructions. The signing keypair (`--keypair`) must hold
   the `AccountExtension` hot role or be the warm/cold admin; assign the role ahead of the
   migration with `velocity-admin auth set-hot-admin accountExtension <pubkey>` so the crank runs
   off a low-stakes hot key rather than an admin key. `--dry-run` prints the affected accounts
   and the rent cost without sending.

5. **Verify.** Re-run each type with `--dry-run`; every scan should report zero accounts below
   target. From here the new fields read as zero-defaults until code writes them.

Only extend types that actually grew. Extending a type whose struct is unchanged is a no-op by
construction, so over-cranking wastes only transaction fees.

## Client behavior

Clients must treat the compiled-in struct size as the number of bytes to read, never as the
expected buffer length. Concretely:

- **TypeScript SDK.** Anchor's borsh coder and the custom `decodeUser` fast path both read
  start-relative offsets, so extended accounts decode unchanged. Pinned by
  `packages/sdk/tests/decode/extendedAccount.ts`.
- **Rust SDK (velocity-rs, keep-rs, swift).** All typed decoding funnels through
  `utils::deser_zero_copy` / `utils::try_deser_zero_copy` or `AccountRef`, which slice
  `data[8..8 + size_of::<T>()]` before casting. Do not call anchor's derived
  `T::try_deserialize` on zero-copy types off-chain; besides the exact-size panic it also panics
  on alignment for 16-aligned structs (see the swift `local_sim` for the pattern when program
  types are unavoidable: trim to `8 + size_of::<T>()`, then copy into `AlignedAccountData`).
- **Never filter by `dataSize`** in `getProgramAccounts`: a hardcoded size silently matches
  nothing after an extension. Filter by discriminator `memcmp` instead (all our maps already do).
- The devnet-gated integration test `tests/velocity/accountExtension.ts` exercises the full loop:
  grow a live `User` and `SpotMarket` beyond every compiled-in struct, then decode them with both
  SDK paths and trade against them on-chain.

## What this does not cover

- **Borsh accounts** (`SignedMsgUserOrders`, `RevenueShareEscrow`, the LP-pool mapping accounts,
  ...). Appending fields to a borsh struct changes deserialization, not just the buffer length;
  old bytes fail to decode as the new type regardless of size. Those accounts version their
  layouts or ship dedicated resize instructions (`resize_signed_msg_user_orders`, ...) and are
  deliberately rejected by `extend_account`.
- **Reordering, inserting, or widening existing fields.** That is a layout break, not an
  extension; existing bytes would be reinterpreted wrongly. The only safe growth is claiming
  trailing padding and appending past the old end.
- **Shrinking.** Never supported; it truncates live data, and the rent refund would let a
  compromised signer drain lamports out of live accounts.
