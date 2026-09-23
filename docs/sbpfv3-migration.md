# SBPFv3 migration

Record of moving Velocity's onchain programs to the SBPFv3 bytecode format
([SIMD-0500](https://github.com/solana-foundation/solana-improvement-documents/blob/main/proposals/0500-sbpf-v3.md),
[Anza migration guide](https://www.anza.xyz/blog/migrating-solana-programs-to-sbpfv3)), and of the
test-runtime change it forced.

SIMD-0500 activates with Agave v4.4. From then on the cluster rejects new deployments, upgrades and
finalizations of any program built for a bytecode version older than SBPFv3. Programs already
deployed keep executing, so nothing breaks the moment it activates. But the mainnet program cannot
be upgraded again until we rebuild it for v3.

## What changed

The Rust source did not change at all. What changed is how it is built and what runs it in tests.

`deploy-scripts/build-sbf.sh` is now the single place that decides the bytecode version
(`--arch v3`), the platform-tools version (v1.57), and each program's feature flags. Every build
path routes through it: the `program:build*` scripts, `build-devnet.sh`, the anchor and vault test
scripts, the anchor-tests CI job, and the nightly fuzz job.

It drives `cargo-build-sbf` directly instead of `anchor build`. Anchor 1.0.2 passes its own
`--tools-version`, pinned to a platform-tools with no sbpfv3 sysroot, so `anchor build -- --arch v3`
fails with ``can't find crate for `core` `` and passing `--tools-version` yourself is rejected as a
duplicate argument. Driving cargo-build-sbf decouples the bytecode version from the Anchor CLI
version. The IDL still comes from `anchor idl build` via `bun run program:idl`.

The build passes `-z defs`. Without it an unresolved syscall links as `call -1`, which builds,
deploys, and only traps when that code path runs on chain. This is the failure mode the Anza guide
warns about, and the flag is the only thing that catches it.

SBF artifacts now land in `target/sbpfv3-solana-solana/`. Every cache-wipe command uses
`target/sbpf*-solana-solana` so it covers both layouts.

## Bytecode results

All six programs build clean for SBPFv3 with `cargo-build-sbf` 4.3.0 and platform-tools v1.57, with
`-z defs` on.

| Program      | v0 size | v3 size | link errors | stack warnings |
| ------------ | ------- | ------- | ----------- | -------------- |
| velocity     | 5.69 MB | 3.89 MB | none        | none           |
| vaults       |         | 1.60 MB | none        | none           |
| jit_proxy    |         | 0.50 MB | none        | none           |
| token_faucet |         | 0.18 MB | none        | none           |
| pyth         |         | 0.07 MB | none        | none           |

The velocity row measures the mainnet flavor (default features). The anchor-test flavor is 4.60 MB.
Every artifact reports ELF `Flags: 0x3`, which is how `llvm-readelf -h` prints what the Anza guide
calls `CPU Version: 3`.

None of the three hazards the guide names applies to us.

- Static syscalls. No `extern "C"` syscall declaration exists anywhere in `programs/`. Everything
  routes through `solana-define-syscall`, and versions 3.0.0, 4.0.1 and 5.0.0 all appear in the
  tree, all at or above the 3.0.0 minimum. A `-z defs` build links with no unresolved symbols.
- Stack frame gaps. v3 makes frames contiguous. That gives more usable stack, but it removes the
  guard region, so an overflow that used to trap now writes into the neighbouring frame instead.
  The failure mode went from loud to silent, which is why anything near the 4096-byte limit wants
  re-testing. No program emits a stack warning under v3.
- Null reads. v3 maps `.rodata` at VM address 0, which v0 left unmapped, so a null read now returns
  data instead of faulting. The programs contain no inline assembly and no raw null-pointer
  construction; every `unsafe` block is a bytemuck zero-copy cast.

velocity loses 32% of its size because v3 drops load-time relocations. 1.8 MB less to upload means
fewer chunked writes per deploy and less rent on the buffer account.

## The test runtime

The old suite ran on `solana-bankrun@0.4.0`, which embeds an Agave 2.x runtime. It rejects a v3
`.so` at the first instruction with `Program is not deployed` / `invalid account data for
instruction`. Note that CLAUDE.md attributes that same text to 896-byte stub `.so` files built with
the wrong feature flags; under v3 it means the runtime does not recognize the bytecode version.

`solana-bankrun` stopped at 0.4.0 and will not gain v3 support. The `litesvm` npm package is its
successor, and it has two lines. 0.x, last released as 0.8.0, keeps the web3.js v1 API that
`anchor-litesvm` still depends on, and it rejects a v3 `.so` outright. 1.x, currently 1.4.1, is
built on litesvm 0.16.0 and agave 4.2.x, and runs v3 correctly.

litesvm 1.x's public API takes `@solana/kit` types, while the SDK and Anchor 1.0's TypeScript client
are both web3.js v1. The adapter avoids that gap entirely rather than converting: LiteSVM's native
binding, reachable as `svm.inner`, takes serialized transaction bytes and raw 32-byte pubkeys, which
is exactly what web3.js hands over. Its public `sendTransaction` is a thin wrapper that decodes the
version and calls `inner.sendLegacyTransaction` / `inner.sendVersionedTransaction`. The previous
adapter reached through bankrun's binding the same way.

So `packages/sdk/src/litesvm/litesvmConnection.ts` replaces
`packages/sdk/src/bankrun/bankrunConnection.ts` with the same shape:

| bankrun                                  | LiteSVM                        |
| ---------------------------------------- | ------------------------------ |
| `startAnchor(path, programs, accounts)`  | `startLiteSVM({ extraPrograms, accounts })` |
| `ProgramTestContext`                     | `LiteSVMContext`               |
| `BankrunContextWrapper`                  | `LiteSVMContextWrapper`        |
| `BankrunConnection`                      | `LiteSVMConnection`            |
| `BankrunProvider` (anchor-bankrun)       | `LiteSVMProvider`              |

`startLiteSVM` loads the Anchor.toml localnet programs plus jit-proxy, searching `target/deploy`
then `tests/fixtures` (where the prebuilt third-party programs live, which is where
solana-program-test found them). `SVM_DEPLOY_DIR` overrides the first directory.

LiteSVM does not provide several things bankrun did implicitly. The adapter supplies them, and
removing any one breaks the suite in a way that points somewhere else first.

- **Payer funding.** `withLamports` raises LiteSVM's airdrop account, and the context payer gets
  1,000,000 SOL. Tests fund individual keypairs with 10,000 to 20,000 SOL in a single transfer, so
  a smaller payer fails with `Transfer: insufficient lamports`. Raising the payer without raising
  the airdrop source is worse. The airdrop then fails silently and every later transaction reports
  `AccountNotFound`.
- **`withNativeMints()`.** bankrun seeded the native SOL mint. Without it the vault suite fails on
  `Mint account So111...112 not found`.
- **Transaction history on, plus a unique blockhash per fetch.** The SDK's retry sender resends
  identical signed bytes while awaiting confirmation, so those must deduplicate or a transaction
  executes twice. Tests that build a second transaction to prove the program rejects it need it to
  differ from the first, or the runtime answers `AlreadyProcessed` and the program never runs. A
  unique blockhash per `getLatestBlockhash` satisfies both, with `withBlockhashCheck(false)` so
  LiteSVM accepts a value it did not issue. `expireBlockhash` is not usable here: it invalidates the
  previous blockhash rather than queueing it, so clients that fetch then send report
  `BlockhashNotFound`.
- **`withSigverify(false)`.** Two suites send versioned transactions whose single required
  signature is all zeros: `placeAndMakeSignedMsgSvm.ts` and `trustedVault.test.ts`. bankrun did not
  verify signatures, so they have always passed. This is a real loss of strictness; turning it back
  on is how to find them, and whether the cause is the tests or the SDK is still open.
- **The clock's timestamp, but not its slot.** LiteSVM boots with `unixTimestamp` 0; suites that
  pin funding and oracle-derived numbers read it, and `userAccount.ts` fails on a one-digit
  difference without it. Leave the slot at LiteSVM's own origin: moving it to bankrun's 1 breaks the
  address-lookup-table program, which rejects a `recent_slot` absent from SlotHashes.
- **SlotHashes.** `warpToSlot` does not add entries, where bankrun's bank did, so the
  address-lookup-table program rejects every `recent_slot` with `<slot> is not a recent slot`. The
  adapter mirrors the sysvar and pushes each advanced slot, capped at Solana's 512.
- **Error message text.** Tests compare `error.message` with strict equality, so the adapter
  renders LiteSVM's napi error values the way Solana's `Display` impl does, for example
  `Error processing Instruction 1: custom program error: 0x1773`. Program logs go on the error as a
  `logs` property, not appended to the message.
- **`rentEpoch` clamping.** web3.js represents a rent-exempt account's `rentEpoch` as a JS number,
  and `u64::MAX` does not fit a double. It rounds up to 2^64, which napi rejects with
  `Bigint too large for u64`. Clamp on the way into `setAccount`.
- **A yield to the event loop after each send.** LiteSVM applies transactions synchronously, so
  without it `await sendTransaction(...)` never yields, timer-driven subscribers never run, and the
  polling account loader serves stale data. Anything deriving an argument from cached state then
  gets it wrong: `initializeSolSpotMarket` reads `getStateAccount().numberOfSpotMarkets`, still sees
  0 after the quote market exists, and the program rejects the PDA with `ConstraintSeeds`. Costs
  about 3 seconds across the suite.
- Advancing the clock one slot per transaction, which the previous adapter also did.

The 107 test files changed only their imports and their `startAnchor('', [], [])` bootstrap line.
They stayed on web3.js v1. `solana-bankrun`, `anchor-bankrun` and the unused `spl-token-bankrun` are
gone from both manifests.

## The verifiable build

`solana-verify` supports `--arch v3`, and the base image's own platform-tools version turns out not
to matter: `cargo-build-sbf` downloads whatever `--tools-version` asks for. Forcing v1.57 inside the
4.1.2 image, which ships v1.54, produces a compliant v3 artifact. Confirmed by building
`token_faucet` that way and checking the ELF `e_flags`.

```bash
solana-verify build --library-name velocity -b solanafoundation/solana-verifiable-build:4.1.2 \
  --arch v3 --cargo-build-sbf-args=--tools-version=v1.57
```

Both flags are part of the recipe, so they belong wherever a release publishes its verification
instructions. Without them `solana-verify` defaults to `--arch v0` and reproduces a different hash.
`fuzz/build-bundle.sh` builds its instrumented coverage `.so` the same way.

Bumping `VERIFIABLE_BUILD_IMAGE` when a 4.3.x tag ships is still worth doing, to drop the override
rather than to unblock anything.

`SOLANA_VERSION` in CI is pinned to `4.3.0`, the first stable Agave whose `cargo-build-sbf` can emit
SBPFv3. 4.2.2 ships 4.1.0 with platform-tools v1.54, under the v1.56 minimum.

## Local setup

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
cargo-build-sbf --install-only --tools-version v1.57
rustup toolchain link 1.95.0-sbpf-solana-v1.57 ~/.cache/solana/v1.57/platform-tools/rust
bun run program:build
llvm-readelf -h target/deploy/velocity.so | grep Flags   # expect 0x3
```

This needs a Solana install of 4.3 or newer on PATH, because older `cargo-build-sbf` binaries do not
accept the v1.57 toolchain. `~/.cargo/bin` must come before `/opt/homebrew/bin` on PATH: the
homebrew cargo does not understand the `+toolchain` directive that `cargo-build-sbf` passes and
reports it as an unknown subcommand.

## Test results

Built with `deploy-scripts/build-sbf.sh test` (SBPFv3), **all 81 files pass**: 386 velocity tests
across the 73 active entries in `run-anchor-tests.sh`, and 46 tests across the 8 vault files, with
zero failures. The 17 files commented out of that list as known-broken were not run.

When a test fails after an adapter change, compare it against the same file on master under
bankrun before treating it as pre-existing. Most of the requirements above first looked like bugs in
the tests.

## Fuzz coverage on v3

The five `e2e-svm*` Crucible harnesses load `target/deploy/velocity.so` directly and already run
litesvm 0.15.2 on agave 4.1.2, so they take v3 bytecode with no changes. Run against a devnet-flavor
v3 build:

| Harness       | Target               | Executions | Crashes |
| ------------- | -------------------- | ---------- | ------- |
| `e2e-svm`     | `invariant_solvency` | 94,003     | 0       |
| `e2e-svm-liq` | `invariant_liq`      | 107,880    | 0       |

Roughly 930 executions per second, reaching 14.9% edge and 25.9% branch coverage with 67 of 90
actions discovered. This is the strongest evidence we have that v3 does not change program
behavior, because it drives real instructions against the real compiled bytecode and checks
solvency and conservation invariants after every action.

These harnesses read `target/deploy/velocity.so` by a hardcoded path and need the devnet flavor,
while the integration tests need the anchor-test flavor. Running both means rebuilding in
between, or pointing the tests elsewhere with `SVM_DEPLOY_DIR`.

The two tiers do not run byte-identical runtimes, and the version numbers are not comparable: the
harnesses use the Rust crate `litesvm` (pinned to `=0.15.2`, because crucible pins it exactly and a
caret range resolves a second copy with a different feature set), while the TypeScript tests use the
npm package `litesvm` 1.4.1, whose native binding is a separate build. What matters is the runtime
underneath, and there they are close: agave 4.1.2 for the harnesses, per `fuzz/e2e-svm/Cargo.lock`,
and 4.2.1 for the npm binding, read out of the shipped `.node`. Both are agave 4.x and both execute
v3. Bumping the harness side is not ours to do alone; it follows crucible's pin.
