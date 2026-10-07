# Building

Read this before you build the programs, change toolchains, touch a feature gate, or debug a
build failure. `AGENTS.md` has the short version of the toolchain rule.

## Toolchain

On Apple Silicon, always use an x86_64 cross-compile toolchain, never a native aarch64 one. Native
ARM toolchains break the memory layout expectations of zero-copy accounts, which must match the
onchain x86_64 representation.

- Anchor 0.29.x branches: `rustup default 1.76.0-x86_64-apple-darwin`
- Anchor 1.0 branches: `rustup default stable-x86_64-apple-darwin`

### Rust version and zero-copy struct alignment

Rust 1.77 corrected `align_of::<u128>()` to 16 bytes on x86_64. The onchain SBF target has always
kept it at 8. Every zero-copy struct in this repo is explicitly padded so that
`(SIZE - 8) % 16 == 0`, and u128 and i128 fields are ordered before any `PoolBalance` fields, which
makes `sizeof` identical on all targets regardless of Rust version.

Develop and test with Rust 1.77 or newer anyway. That way x86_64 exercises real 16-byte u128
alignment, and a future struct change that breaks the invariant fails the `const_assert_eq!` guards
locally instead of diverging onchain. Anchor 1.0 branches require Rust 1.93 or newer. The Anchor
1.0 MSRV is 1.89, but velocity uses `Box::new_zeroed` (1.92) and the `rust/` workspace locks
`solana-syscalls` 4.2 (1.93). CI pins 1.95.0. [`../alignment-and-native-offsets.md`](../alignment-and-native-offsets.md) has
the full invariant rules and covers adding fields to zero-copy structs.

## Program build scripts

Use the `program:*` scripts in the root `package.json` for the Solana programs. They encode the
correct feature flags so you do not have to remember them.

```bash
bun run program:build           # all four test programs + IDL/types synced into packages/sdk/src/idl/
bun run program:idl             # IDL/types only, no SBF build. Fast path for layout/name changes
bun run program:build:devnet    # deployable devnet .so (wraps deploy-scripts/build-devnet.sh)
bun run program:build:mainnet   # mainnet .so (default features: production gates on, devnet ixs compiled out)
```

`program:build` and `program:idl` use `--no-default-features --features no-entrypoint,anchor-test`
for velocity. `program:build` gives the other three programs their own flags (see `build-sbf.sh`).
That is required even though `declare_id!` is now unconditional, because default features include
`mainnet-beta`. `mainnet-beta` compiles out the devnet-only instructions, including
`force_wipe_accounts_devnet`, which `wipe-devnet.ts` calls through the SDK IDL, and it switches
`ids.rs` to the mainnet constants. `program:idl` runs `anchor idl build` under `cargo test` with the
host toolchain, which avoids the bundled-cargo problems described below.

A full `anchor build` already emits both `target/idl/velocity.json` and
`target/types/velocity.ts`, and the scripts only copy them into `packages/sdk/src/idl/`, so no
separate `anchor idl build` or `anchor idl type` step is needed after a full build.

To build the TypeScript workspace, run `bun run build` (which is `turbo run build`), or
`bunx turbo run build --filter=@velocity-exchange/sdk` for the SDK and its dependencies.

## Post-audit feature gates (`isolated-position`, `vlp-hedge`)

The instructions for isolated perp positions and for the VLP hedge and LP-pool component are
compiled out of mainnet builds, meaning the default feature set, pending audit. `anchor-test`
implies both features, so `program:build`, `program:idl` and all tests keep them. `build-devnet.sh`
enables them explicitly, so devnet keeps them live.

Only the instructions are gated. All state stays compiled into every build, including
`PerpPosition.isolated_position_scaled_balance`, `PerpMarket.hedge_config` and the LP-pool
accounts, along with the interior logic, so account layouts never diverge between flavors.

A gated instruction must not share a `#[derive(Accounts)]` struct with an ungated one. Anchor's
`cpi` module deduplicates the per-struct re-export and inherits the cfg, so the ungated
instruction's cpi accounts type disappears. The build then breaks only when the `cpi` feature is on,
and a plain `cargo check -p velocity` does not catch it. Run `cargo check -p vaults` as well, or
`cargo check -p velocity --features cpi`.

This is why five lp-pool admin config instructions stay ungated:
`update_perp_market_lp_pool_id`, `update_perp_market_lp_pool_paused_operations`, and the three
`update_feature_bit_flags_*_lp_pool` instructions. They are inert config writes whose readers are
compiled out.

To enable either subsystem on mainnet, add the features to the mainnet build invocation and upgrade
in place. When you touch either subsystem, check that both flavors compile:
`cargo check -p velocity` for the gated build, and
`cargo check -p velocity --no-default-features --features no-entrypoint,anchor-test` for the
enabled one.

## SBPFv3

The programs build for SBPFv3, which SIMD-0500 makes the only deployable bytecode format from Agave
v4.4 on. `deploy-scripts/build-sbf.sh` owns the bytecode version (`--arch v3`), the platform-tools
version, and each program's feature flags, and every build path routes through it. It calls
`cargo-build-sbf` directly instead of `anchor build`, because Anchor 1.0.2 passes its own
`--tools-version` pinned to a platform-tools with no sbpfv3 sysroot, so `anchor build -- --arch v3`
fails with ``can't find crate for `core` ``. The IDL still comes from `anchor idl build`
(`bun run program:idl`).

Two things to keep:

- The build passes `-z defs`. Without it an unresolved syscall links as `call -1`, which builds,
  deploys, and only traps when that path runs on chain. Never remove it.
- SBF artifacts now land in `target/sbpfv3-solana-solana/`, not `target/sbpf-solana-solana/`. The
  cache-wipe commands use `target/sbpf*-solana-solana` so they cover both.

`deploy-scripts/assert-sbpf-version.sh` fails unless a `.so` carries the expected version. It runs
at the end of every build and again in the devnet buffer deploy scripts, because a v0
artifact builds and deploys fine today and becomes un-upgradable the day SIMD-0500 activates.

The verifiable build emits v3 as well. The image tag does not decide the bytecode version.
`cargo-build-sbf` downloads whatever `--tools-version` asks for, so the `verified-build` job forces
v1.57 inside the 4.1.2 image, which ships v1.54. Anyone reproducing a release hash has to pass the
same flags, so keep them next to the image tag in any verification instructions.

```bash
solana-verify build --library-name velocity -b <image> \
  --arch v3 --cargo-build-sbf-args=--tools-version=v1.57
```

[`../sbpfv3-migration.md`](../sbpfv3-migration.md) has the measurements and what is left.

## Rust SDK and keeper workspace (`rust/`)

`rust/` is a second Cargo workspace holding the imported Rust crates: `velocity-rs` (Rust SDK),
`keep-rs` (keeper bots, binary `keeprs`), and `swift` (tx server, binary `swift-server`). It is
separate from the program workspace (root `Cargo.toml` has `exclude = ["rust"]`) so its split
solana 4.2 crate tree never unifies with the program's SBF build. It has its own `rust/Cargo.lock`
and builds into `rust/target/` (via `rust/.cargo/config.toml`), never clobbering `./target`.
Check it with `cargo check --manifest-path rust/Cargo.toml` (or `bun run rust:build`).

- These crates consume the velocity program as a host library path-dep:
  `drift = { package = "velocity", path = "../../programs/velocity", ... }`. That host build is
  independent of `cargo build-sbf`.
- `velocity-rs/build.rs` regenerates `velocity-rs/crates/src/velocity_idl.rs` from
  `packages/sdk/src/idl/velocity.json`, the same file the TypeScript SDK consumes.
  `bun run program:idl` regenerates that IDL, and the next `cargo build` or `cargo check` of the
  rust workspace picks it up. `velocity_idl.rs` is committed. When the IDL json is absent (velocity-rs
  consumed as a git dep, vendored, or packaged), the build script falls back to the committed file.
  It only rewrites the file when the generated content differs, so read-only source dirs such as
  `cargo vendor` and Nix build cleanly. The `rust-workspace-check` CI job fails if the committed
  `velocity_idl.rs` is out of sync with the IDL.
- `keep-rs`'s `[patch.crates-io]` and `swift`'s `[profile.dev.package]` are hoisted into
  `rust/Cargo.toml`, because Cargo only honors patches and profiles at the workspace root.
  `keep-rs/vendor/pyth-lazer-protocol` is un-ignored in `.gitignore`.
- The velocity fork removed some upstream features (IF-rebalance /
  `ProtocolIfSharesTransferConfig`, gov-token staking). When importing newer velocity-rs, keep-rs
  or swift, expect to drop references to removed types (see the import commits for the pattern).

## macOS build environment

Fresh setups regularly hit the problems below. If you see one of these errors, apply the matching
fix before debugging anything else.

### `c/blake3_impl.h:4:10: fatal error: 'assert.h' file not found`

Seen during `anchor build`. The Solana platform-tools clang has no built-in macOS SDK path, so it
can't find system headers.

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
```

Prefix the build command with it, or add it to your shell profile.

### `feature 'edition2024' is required ... not stabilized in this version of Cargo (1.84.0)`

Seen when downloading `toml_datetime`, `wincode` or `toml_parser`. The bundled cargo in older
platform-tools (v1.51 ships cargo 1.84) cannot parse `edition2024` deps. Upgrade platform-tools.
SBPFv3 needs v1.56 or newer, and the repo pins v1.57.

```bash
cargo-build-sbf --install-only --tools-version v1.57
```

Run that once. Later `anchor build` invocations use the new toolchain. Check with
`cargo-build-sbf --version`.

### `could not execute process .../1.89.0-sbpf-solana-v1.52/bin/rustc (never executed)`

Seen during an SBF build. The platform-tools payload is present under `~/.cache/solana/<version>/`
but its rustup toolchain link is missing. `cargo-build-sbf` picks its own default version rather
than whichever one you last installed, so having v1.57 linked does not help when it wants v1.52.
Link the version it asks for:

```bash
rustup toolchain link 1.95.0-sbpf-solana-v1.57 ~/.cache/solana/v1.57/platform-tools/rust
```

Substitute the version from the error path. This is machine state, not repo state, so it recurs on
any fresh worktree or new machine until linked.

### `no such command: +1.95.0-sbpf-solana-v1.57`

Seen with any `+<toolchain>` name during an SBF build. `cargo-build-sbf` invokes
`cargo +<toolchain>`, which works only when `cargo` is the rustup shim. A homebrew cargo at
`/opt/homebrew/bin/cargo` earlier on PATH does not understand the directive. Put `~/.cargo/bin`
ahead of `/opt/homebrew/bin`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

### `Access violation in unknown section at address 0x80 of size 8`

The program panics with this (or a similar address) at runtime, on instructions that touch types
you didn't change. This is almost always stale SBF build artifacts after a `Cargo.lock` dependency
change. SBF caches compiled `.rlib`s under `target/sbpfv3-solana-solana/`, and the cache key does
not catch every dependency-resolution change, so the resulting `.so` loads but reads and writes
wrong offsets. Whenever `Cargo.lock` versions change (after `cargo update`, or after switching
branches with different lockfiles), run:

```bash
rm -rf target/sbpf*-solana-solana target/deploy
bash deploy-scripts/build-sbf.sh test
```

### `Access violation in stack frame 3 at address 0x2000...`

This is a **platform-tools v1.52 miscompile**, not a stale cache: v1.52 (the default bundled with `cargo-build-sbf` 3.1.14, which plain `anchor build` uses) emits velocity code that overflows a 4KB stack frame at runtime; v1.54 and later compile the same code correctly. Any velocity `.so` that will actually be _executed_ (validator deploys, the e2e localnet harness, devnet buffers) must pin the toolchain explicitly rather than rely on a default. `deploy-scripts/build-sbf.sh` pins v1.57 and every build path routes through it:

```bash
bash deploy-scripts/build-sbf.sh test velocity
```

The litesvm/bankrun suites can mask this: they exercise only the instructions each test calls, and older runtimes were lenient. `integration-tests/tests/init_probe.rs` pins the real `initialize_user_stats` path so a miscompiled `.so` fails fast.
