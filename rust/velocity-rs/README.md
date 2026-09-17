# velocity-rs

Rust SDK for offchain clients of the [Velocity](https://velocity.exchange) protocol on Solana.

`velocity-rs` lives in the [velocity-v1](https://github.com/velocity-exchange/velocity-v1)
monorepo, under `rust/velocity-rs`.

## Install

The crate is consumed as a git dependency (it is not published to crates.io). Cargo
locates the package inside the monorepo automatically:

```toml
[dependencies]
velocity-rs = { git = "https://github.com/velocity-exchange/velocity-v1", rev = "<commit-sha>" }
```

Pin a `rev`, or a `tag` once tagged releases exist. A dependency on the default branch lets
every `cargo update` pull a breaking change.

### Requirements

- **Rust ≥ 1.89**, the Anchor 1.0 MSRV. The monorepo CI builds with recent stable.
- **Apple Silicon:** use an x86_64 toolchain and Rosetta. See [Setup](#setup). Native aarch64
  toolchains are unsupported for code that deserializes the program's zero-copy accounts.
- velocity-rs is built on the **solana `4.x` RPC and transaction crate line**
  (`solana-rpc-client`, `solana-pubkey`, `solana-transaction`, …). That line decodes SIMD-0385
  transaction v1. A few split crates that never left 3.x stay there, such as
  `solana-commitment-config`, `solana-keypair`, and `solana-signature`. An app pinned to the
  legacy `solana-sdk 1.x/2.x` types hits type mismatches at the API boundary.

Consumers need no FFI layer, no submodule, and no build-time codegen. The crate depends on the
`velocity` program crate as a host-library path-dep inside the repo. The IDL-derived types in
`crates/src/velocity_idl.rs` are committed, and CI keeps them in sync. The build script
regenerates them only inside the monorepo, where the canonical IDL is present, and rewrites the
file only when the content changed. Read-only and vendored checkouts such as `cargo vendor` and
Nix therefore build cleanly.

## Use

`VelocityClient` reads velocity program accounts and builds transactions. It caches live account
updates from a subscription and exposes them through accessor methods. Subscribe over WebSocket or
over gRPC.

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

Warning: native aarch64 toolchains are unsupported. The program's zero-copy account structs must
match the on-chain x86_64 SBF memory layout. An aarch64 build can fail at runtime with a
deserialization error such as `InvalidSize`.

### Linux

x86_64 with stable Rust ≥ 1.89. No extra setup is needed.

## Local development

`velocity-rs` consumes the `velocity` program crate as a host-library path-dep at
`../../programs/velocity`. There is no FFI layer, no `drift-ffi-sys`, and no git submodule.

Clone the monorepo:

```bash
git clone https://github.com/velocity-exchange/velocity-v1 &&\
cd velocity-v1/rust/velocity-rs
```

Build:

```bash
cargo check
```

The `rust/` directory is its own Cargo workspace, separate from the program workspace at the repo
root. It has its own lockfile and its own `rust/target/` build directory.

## Updating IDL types

`crates/src/velocity_idl.rs` is generated from the canonical program IDL at
`packages/sdk/src/idl/velocity.json`, the same file the TypeScript SDK uses. The generated file is
committed. `build.rs` regenerates it only when the canonical IDL is present, which means inside the
monorepo, and rewrites it only when the content changed. CI fails if the committed file is out of
sync with the IDL.

To refresh it after a program change, from the monorepo root:

```shell
bun run program:idl   # regenerates packages/sdk/src/idl/velocity.json
cargo check --manifest-path rust/Cargo.toml   # build.rs rebuilds velocity_idl.rs from it
# commit both files
```
