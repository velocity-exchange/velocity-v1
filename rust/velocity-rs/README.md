# velocity-rs

High performance Rust SDK for building offchain clients for the
[Velocity](https://velocity.exchange) protocol on Solana.

`velocity-rs` lives in the [velocity-v1](https://github.com/velocity-exchange/velocity-v1)
monorepo, under `rust/velocity-rs`.

## Install

The crate is consumed as a git dependency (it is not published to crates.io). Cargo
locates the package inside the monorepo automatically:

```toml
[dependencies]
velocity-rs = { git = "https://github.com/velocity-exchange/velocity-v1", rev = "<commit-sha>" }
```

Pin a `rev` (or a `tag`, once tagged releases exist) — depending on the default branch
means every `cargo update` can pull breaking changes.

### Requirements

- **Rust ≥ 1.89** (Anchor 1.0 MSRV; the monorepo CI builds with recent stable).
- **Apple Silicon:** use an x86_64 toolchain (`rustup override set stable-x86_64-apple-darwin`,
  Rosetta required) — see [Setup](#setup). Native aarch64 toolchains are unsupported for
  code that deserializes the program's zero-copy accounts.
- velocity-rs is built on the **solana `4.x` RPC/tx crate line** (`solana-rpc-client`,
  `solana-pubkey`, `solana-transaction`, …), which is what decodes SIMD-0385 transaction v1.
  A few split crates that never left 3.x (`solana-commitment-config`, `solana-keypair`,
  `solana-signature`, …) stay there. Apps pinned to the legacy `solana-sdk 1.x/2.x` types
  will hit type mismatches at the API boundary.

No FFI layer, no submodules, no build-time codegen for consumers: the crate depends on
the `velocity` program crate as a plain host-library path-dep within the repo, and the
IDL-derived types (`crates/src/velocity_idl.rs`) are committed and kept in sync by CI.
The build script only regenerates them when building inside the monorepo with the
canonical IDL present, and never rewrites the file unless its content changed — so
read-only and vendored checkouts (`cargo vendor`, Nix) build cleanly.

## Use

The `VelocityClient` struct provides methods for reading velocity program accounts and crafting transactions.
It is built on a subscription model where live account updates are transparently cached and made accessible via accessor methods.
The client may be subscribed either via Ws or gRPC.

```rust
use velocity_rs::{AccountFilter, VelocityClient, Wallet, grpc::GrpcSubscribeOpts};
use solana_sdk::signature::Keypair;

async fn main() {
    let client = VelocityClient::new(
        Context::MainNet,
        RpcClient::new("https://rpc-provider.com"),
        Keypair::new().into(),
    )
    .await
    .expect("connects");

    // Subscribe via WebSocket
    //
    // 1) Ws-based live market and price changes
    let markets = [MarketId::spot(1), MarketId::perp(0)];
    client.subscribe_markets(&markets).await.unwrap();
    client.subscribe_oracles(&markets).await.unwrap();
    client.subscribe_account("SUBACCOUNT_1");

    // OR 2) subscribe via gRPC (advanced)
    // gRPC automatically subscribes to all markets and oracles
    client.grpc_subscribe(
      "https://grpc.example.com".into(),
      "API-X-TOKEN".into(),
      GrpcSubscribeOpts::default()
        .user_accounts("SUBACCOUNT_1", "SUB_ACCOUNT_2")
        .on_slot(move |new_slot| {
          // do something on slot
        })
        .on_account(
          AccountFilter::partial().with_discriminator(User::DISCRIMINATOR),
          move |account| {
              // do something on user account updates
          })
    ).await;

    //
    // Fetch latest values
    ///
    let sol_perp_price = client.oracle_price(MarketId::perp(0));
    let subaccount_1: User = client.try_get_account("SUBACCOUNT_1"));
```

## Setup

### Mac (Apple Silicon)

Install Rosetta and use an x86_64 Rust toolchain:

```bash
softwareupdate --install-rosetta

rustup toolchain install stable-x86_64-apple-darwin --force-non-host
rustup override set stable-x86_64-apple-darwin
```

⚠️ Native aarch64 toolchains are unsupported: the program's zero-copy account structs
must match the on-chain (x86_64/SBF) memory layout, and aarch64 builds can fail at
runtime with deserialization errors like `InvalidSize`.

### Linux

x86_64 with stable Rust ≥ 1.89 — no special setup.

## Local Development

`velocity-rs` consumes the `velocity` program crate directly as a host-library path-dep
(`../../programs/velocity`). There is no FFI layer, no `drift-ffi-sys`, and no git submodule.

**clone the monorepo**

```bash
git clone https://github.com/velocity-exchange/velocity-v1 &&\
cd velocity-v1/rust/velocity-rs
```

**build**

```bash
cargo check
```

The `rust/` directory is its own Cargo workspace (separate from the program workspace at
the repo root) with its own lockfile and `rust/target/` build dir.

## Updating IDL types

`crates/src/velocity_idl.rs` is generated from the canonical program IDL
`packages/sdk/src/idl/velocity.json` (the same file the TypeScript SDK uses). The
generated file is **committed**; `build.rs` regenerates it only when the canonical IDL
is present (i.e. inside the monorepo) and only rewrites it when the content changed.
CI fails if the committed file is out of sync with the IDL.

To refresh it after a program change, from the monorepo root:

```shell
bun run program:idl   # regenerates packages/sdk/src/idl/velocity.json
cargo check --manifest-path rust/Cargo.toml   # build.rs rebuilds velocity_idl.rs from it
# commit both files
```
