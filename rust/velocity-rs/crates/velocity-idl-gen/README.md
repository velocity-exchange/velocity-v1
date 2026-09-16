# velocity-idl-gen

Generates Rust anchor structs from IDL json.

There is no need to run this manually for velocity-rs. Its `build.rs` calls
`generate_rust_types` on the canonical program IDL and writes
`crates/src/velocity_idl.rs`.

This exists here rather than as another project for two reasons:

1. It marks the generated structs `#[repr(C)]`. Other IDL generation tools do not offer
   that, and the accounts velocity-rs reads are zero-copy, so their field layout has to
   match the program's C representation byte for byte.
2. It does not rely on anchor's vendored solana crates. Anchor pins older versions of
   them, and velocity-rs upgrades to the latest solana crates on its own schedule.

## Dev note

The code assumes the underlying types serialize and deserialize identically across solana
crate versions, so that `solana_sdk_1.16::Pubkey == solana_sdk_2.x::Pubkey ==
anchor_lang::solana_sdk::Pubkey`. That is what lets the generated IDL code ignore which
version of those crates anchor brings in.
