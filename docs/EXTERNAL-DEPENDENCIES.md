# External dependencies

A complete inventory of what the on-chain programs in this repo depend on, at runtime and at
build time, with the trust assumption and failure mode for each.

Two questions this doc is meant to answer directly:

- **What does Velocity trust that it does not control?**
- **What breaks, and how badly, when each of those things misbehaves?**

## Scope

**Covered:** the three programs deployed from this repo (`velocity`, `vaults`, and `jit-proxy`),
including every program they call, every account owner they deserialize, and their full resolved
crate graph.

**Not covered:** the off-chain services and clients, which live in sibling repos and are not part
of the on-chain trust surface. §5 lists the off-chain components Velocity depends on for
*liveness* and points at where they live, but does not enumerate their dependencies. The
TypeScript SDK's npm tree is likewise out of scope; it is a client library, and a compromise
there does not move funds on its own.

Program IDs below are mainnet unless noted. Every ID was read from source, not from memory;
see §7 for how to re-verify.

---

## 1. Programs Velocity calls (CPI out)

These are the only programs the deployed code invokes. There are five, and four of them are
Solana or SPL infrastructure.

| Program | ID | Caller | Used for | Trust assumption | Failure mode |
|---|---|---|---|---|---|
| System | `11111111111111111111111111111111` | `velocity` | PDA create / allocate / assign / transfer (`controller/pda.rs`) | Part of the runtime; not independently trusted | None separable from the chain halting |
| SPL Token | `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA` | `velocity`, `vaults` | `transfer_checked`, `burn`, `mint_to`, `close_account`, `initialize_account3` (`controller/token.rs`) | Program is frozen and heavily audited | A defect would be systemic to Solana; Velocity has no independent mitigation |
| SPL Token-2022 | `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb` | `velocity`, `vaults` | Same operations, through `anchor_spl::token_interface` | Upgradeable by the SPL authority; extension semantics behave as documented | See "Token-2022 extensions" below; this is the largest CPI-side risk |
| Associated Token Account | `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL` | `velocity` | Protocol-fee withdrawal accounts; also whitelisted inside swap flows | Standard derivation | Withdrawal instructions fail; no fund risk |
| Metaplex Token Metadata | `metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s` | `vaults` only | `create_metadata_accounts_v3` in `initialize_tokenized_vault_depositor` | Metaplex upgrade authority | Tokenized vault depositors cannot be created. Existing vault funds and the entire perps program are unaffected |

`velocity` is a CPI *target* of `vaults` and `jit-proxy` (`J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ`),
both of which depend on it with the `cpi` feature. That direction is inbound and adds no external
trust.

### Token-2022 extensions

Token-2022 is a single program ID hiding a variable feature set, so it deserves its own note.
Two extensions change what a transfer means:

- **Transfer fee.** The recipient receives less than the amount sent, which would silently
  under-credit a deposit. Guarded by `validate_mint_fee` (`controller/token.rs:219`).
- **Transfer hook.** `transfer_checked_with_transfer_hook` (`controller/token.rs:234`) forwards
  `remaining_accounts` into the transfer instruction, so a mint configured with a hook pulls an
  **arbitrary third-party program** into Velocity's CPI. That program is not enumerable in advance:
  it is whatever the mint author set.

The mitigation is administrative rather than programmatic. Only mints an admin lists as a spot
market can reach these paths, so the real control is spot-market listing review. Treat "does this
mint have a transfer hook, and what does the hook program do?" as a required listing check.

---

## 2. Programs Velocity reads but never calls

Oracle prices are read by deserializing account data. No CPI is involved, so the oracle program
cannot execute code in Velocity's context. But its data drives every margin, liquidation, and
funding decision, which makes this the highest-consequence dependency in the system.

### 2.1 Pyth Lazer: primary price source

| | |
|---|---|
| Trusted signer storage | `3rdJbqfnagQ4yx9HXJViD4zc4xpiSqmFsKpPuSCQVyQL` (address-locked in `instructions/pyth_lazer_oracle.rs`) |
| Reference program ID | `pytd2yyk641x7ak7mkaasSJVXh6YYZnC7wTmtgAyxPt` (`ids.rs::pyth_lazer_program`) |
| Path | Off-chain signed message → Ed25519 sigverify instruction → `update_pyth_lazer_oracle` → Velocity-owned `PythLazerOracle` PDA |

Prices arrive as messages signed off-chain by Pyth's signer set. `handle_update_pyth_lazer_oracle`
verifies the signature by introspecting the preceding Ed25519 instruction through the instructions
sysvar, checks it against the signer set in the Storage account, then writes the price into a PDA
that Velocity owns. Reads at fill time hit Velocity's own account.

**Trust assumptions:** Pyth's signer keys are not compromised; Pyth's publishers report honestly;
the Storage account's signer set is correct.

**Failure modes:**

| Failure | Effect | Mitigation |
|---|---|---|
| Feed freezes (publisher or relay stalls) | Prices go stale while markets move | `PYTH_LAZER_MAX_STALENESS_SECONDS`; oracle validity gating per `VelocityAction`; monotonic `next_timestamp` rejection. This class of failure has occurred in production; see the filler Lazer feed watchdog work |
| Signer key compromise | Attacker sets an arbitrary price and drains via liquidations or mispriced fills | Oracle guard rails: confidence-interval multiplier, TWAP price bands, divergence checks. These bound but do not eliminate the damage |
| Velocity's keeper stops cranking updates | Prices go stale even though Pyth is healthy | Liveness dependency on our own infrastructure, not on Pyth; see §5 |

Note the composite dependency: a healthy price requires **both** Pyth to publish **and** a Velocity
keeper to land the update transaction.

### 2.2 Pyth V1 push oracle

`FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH` (mainnet), `gSbePebfvPy7tRqimPoVecS2UsBvYv46ynrzWocc92s`
(devnet).

The **only** external program allowed to own an oracle account: `EXTERNAL_ORACLE_PROGRAM_IDS` in
`state/oracle_map.rs:47` is a one-element list. Read by direct deserialization.

**Trust assumption:** Pyth's on-chain program and its publisher set.
**Failure modes:** stale price, wide confidence, or a divergent aggregate. All three are handled by
the same guard rails as Lazer: `get_price_data_and_validity`, `is_oracle_valid_for_action`,
per-market `is_recent_oracle_valid` and `get_max_confidence_interval_multiplier`, and TWAP-based
price bands.

### 2.3 Sources that are not external

Listed here so the inventory is complete and nobody mistakes them for third-party inputs.

| Source | Owner | Notes |
|---|---|---|
| `PrelaunchOracle` | Velocity | Admin/keeper-written account for pre-launch markets |
| MM oracle | Velocity | Keeper-posted price on `PerpMarket`; used only when it beats the exchange oracle on validity, sequence ID, and divergence checks (`state/oracle.rs`) |
| `QuoteAsset` | n/a | Hardcoded to $1; no account read |

---

## 3. Programs Velocity composes with but never invokes

`begin_swap` / `end_swap` (`instructions/user.rs`), `liquidate_spot_with_swap`
(`instructions/keeper.rs`), and the VLP hedge admin path let a caller route through an external
DEX inside the same transaction. **Velocity does not CPI into any of them.** It reads the
instructions sysvar and rejects the transaction unless every instruction between begin and end
belongs to a hardcoded whitelist.

| Program | ID | Notes |
|---|---|---|
| Jupiter v6 | `JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4` | |
| Jupiter v4 | `JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB` | |
| Jupiter v3 | `JUP3c2Uh3WA4Ng34tw6kPd2G4C5BB21Xo36Je1s32Ph` | |
| DFlow aggregator v4 | `DF1ow4tspfHX9JwWJsAb9epbkA8hmpSEAtxXy1V27QBH` | |
| Titan Argos v1 | `T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT` | |
| Serum / OpenBook | `srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX` | Swap routing only. External **spot fulfillment** venues were removed; only the internal AMM/DLOB remains (`state/fulfillment_params/mod.rs`) |
| Marinade | `MarBmsSgKXdrN1egZf5sqe1TMai9K1rChYNDJgjq7aD` | Allowed only when no delegate is signing |
| Lighthouse | `L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95` | Assertion program; allowed after `end_swap` |
| ATA, Token, Token-2022 | see §1 | Allowed only when no delegate is signing |

Source of truth: `ids.rs::WHITELISTED_SWAP_PROGRAMS` plus the per-instruction additions in
`handle_begin_swap` and `vlp/hedge/admin.rs`.

**Trust assumption: none for solvency.** Velocity does not trust the venue to price fairly. It
measures the user's token balances before and after and applies `validate_price_bands_for_swap`,
so a bad or malicious route fails the check rather than settling at a bad price.

**Failure mode:** a venue going down or changing its program ID makes swaps and swap-based
liquidation unavailable. Positions and balances are unaffected. Adding a venue requires a program
upgrade, which is a deliberate design choice: the whitelist cannot be extended by admin
configuration.

---

## 4. Solana runtime surface

| Dependency | Used for |
|---|---|
| Clock sysvar | Timestamps and slots throughout; read by syscall, not as an account, in newer handlers |
| Rent sysvar | PDA funding in `controller/pda.rs` |
| Instructions sysvar | Ed25519 signature verification for Pyth Lazer and signed-message (swift) orders; whitelist introspection for swaps |
| Ed25519 native program | Signature verification instruction that must precede `update_pyth_lazer_oracle` and signed-message order placement |

Runtime feature-gate activations and validator upgrades are an implicit dependency for every
Solana program, Velocity included. The zero-copy alignment invariants in
[`alignment-and-native-offsets.md`](./alignment-and-native-offsets.md) exist because of exactly
this class of change.

---

## 5. Off-chain dependencies (liveness, not solvency)

None of these can move funds on their own, because every action they take goes through a signed
instruction the program validates. But the protocol does not function correctly without them.

| Component | Where it lives | What stops if it stops |
|---|---|---|
| Oracle cranker | `rust/keep-rs`, internal bot suite | Prices go stale; markets gate into reduced functionality |
| Filler / keeper bots | `apps/keeper-bots-v2`, `rust/keep-rs` | Orders stop filling; funding and PnL settlement stall |
| Liquidator | Internal bot suite (off-repo) | Underwater positions are not liquidated; bad debt accrues |
| Swift server | `rust/swift` | Signed-message order submission stops |
| DLOB server | `apps/dlob-server` | Clients lose the order book view and auction-param endpoint |
| vAMM crank | Internal service (off-repo) | vAMM spread does not widen on anomalous flow |
| Pyth Lazer publisher infrastructure | Third party | See §2.1 |
| Solana RPC providers | Third party | Bots and clients cannot submit transactions |

Deployment and monitoring for these live in `infrastructure-v3`.

---

## 6. Build-time dependencies (supply chain)

### 6.1 Direct dependencies of `velocity`

| Crate | Version | Purpose |
|---|---|---|
| `anchor-lang` | 1.0.2 | Framework: accounts, serialization, CPI |
| `anchor-spl` | 1.0.2 | Token / Token-2022 / ATA interfaces |
| `solana-program` | 3.0.0 | Runtime bindings |
| `borsh` | 1.6.1 | Serialization |
| `bytemuck` | 1.25.0 | Zero-copy account casting |
| `pyth-client` | 0.2.2 | Pyth V1 account layouts |
| `pyth_lazer` | local (`programs/pyth-lazer`) | Lazer message, payload, signature, storage types. Linked as a library, **not** a CPI target |
| `uint` | 0.9.5 | 256-bit integer math |
| `num-traits`, `num-integer` | 0.2.19, 0.1.46 | Numeric traits |
| `arrayref` | 0.3.9 | Slice-to-array conversion |
| `base64` | 0.13.1 | Encoding |
| `byteorder` | 1.5.0 | Endian conversion |
| `enumflags2` | 0.6.4 | Bitflag enums (paused operations, feature bits) |
| `hex` | 0.4.3 | Encoding |
| `static_assertions` | 1.1.0 | Compile-time layout guards |
| `solana-security-txt` | 1.1.2 | On-chain contact metadata |

The `fuzz-fixtures` feature additionally pulls in `bytes` and the local `pyth` crate. It is off in
every SBF, devnet, and mainnet build invocation, so it never reaches a deployed artifact.

### 6.2 Transitive closure

| Program | Unique crates in the normal dependency graph |
|---|---|
| `velocity` | 233 (231 external; the other two are `velocity` itself and the local `pyth_lazer`) |
| `vaults` | 246 (adds `mpl-token-metadata` and its dependencies, plus the local `velocity-macros`) |

The graph is dominated by three roots: `anchor-lang`, `anchor-spl`, and `solana-program`. Nearly
every `solana-*` and `spl-*` entry in Appendix A arrives through one of them. The non-Solana,
non-Anchor crates that are genuinely third-party surface are the direct dependencies in §6.1 plus
their small tails (`serde`, `thiserror`, `bytemuck`, the `digest`/`sha2` hashing stack, and the
`curve25519-dalek` / `k256` / `zeroize` cryptography stack that `solana-program` brings in).

23 crates are proc macros. They execute at compile time and do not ship in the `.so`, but they do
run code on build machines, so they belong in the supply-chain picture:

`anchor-attribute-*`, `anchor-derive-*`, `borsh-derive`, `bytemuck_derive`, `derive_more`,
`enumflags2_derive`, `num-derive`, `num_enum_derive`, `rustversion`, `serde_derive`,
`solana-sdk-macro`, `spl-discriminator-derive`, `strum_macros`, `thiserror-impl` (v1 and v2),
`zeroize_derive`.

### 6.3 Pinning and verification

- Versions are pinned by the committed root `Cargo.lock`. The `rust/` workspace has its own
  separate `Cargo.lock`; the two never unify (root `Cargo.toml` sets `exclude = ["rust"]`).
- Reproducible builds go through `deploy-scripts/verified-build.sh`, which wraps
  `solana-verify build --library-name velocity`. Verify a deployed artifact against source with
  the matching `solana-verify verify-from-repo`.
- `deploy-scripts/verify-buffer.sh` checks a pending upgrade buffer before the swap.

---

## 7. Regenerating and re-verifying this doc

Run these from the repo root. Update the counts in §6.2 and the list in Appendix A whenever
`Cargo.lock` changes.

```bash
# Transitive crate set for a program (default features)
cargo tree -p velocity -e normal --prefix none | sed 's/ (\*)$//;s/ (proc-macro)$//' | sort -u

# Direct dependencies only
cargo tree -p velocity -e normal --depth 1 --prefix none

# Proc-macro crates
cargo tree -p velocity -e normal --prefix none | grep '(proc-macro)'

# Every hardcoded external program ID
cat programs/velocity/src/ids.rs

# Every CPI the program makes
grep -rn "invoke_signed\|CpiContext" --include="*.rs" programs/velocity/src programs/vaults/src
```

Program IDs for SPL and Metaplex are declared in the dependency crates themselves
(`spl-token-interface`, `spl-token-2022-interface`, `spl-associated-token-account-interface`,
`mpl-token-metadata`), not in Velocity's source. Check there rather than trusting this table
after a dependency bump.

### Declared but unused

`ids.rs` still declares four program IDs that nothing references. They are **not** dependencies,
and they should not appear in a dependency questionnaire answer:

| Constant | ID | Status |
|---|---|---|
| `switchboard_program` | `SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f` | Unreferenced |
| `switchboard_on_demand` | `SBondMDrcV3K4kxZR1HNVT7osZxAHVHgYXL5Ze1oMUv` | Unreferenced |
| `wormhole_program` | `HDwcJBJXjL9FpJ7UBsYBtaDjsBUhuLCUYoz3zr8SWWaQ` | Unreferenced; residue from the removed Pyth-pull path |
| `velocity_oracle_receiver_program` | `G6EoTTTgpkNBtVXo96EQp2m6uwwVh2Kt6YidjkmQqoha` | Unreferenced; same origin |

Correspondingly, `OracleSource::DeprecatedSwitchboard`, `DeprecatedSwitchboardOnDemand`, and the
four `*Pull` variants all return `ErrorCode::InvalidOracle` from `get_oracle_price`. They exist
only to hold their ABI discriminants stable.

**There is no bridge dependency.** Velocity holds no wrapped or bridged assets by protocol design,
performs no cross-chain messaging, and the Wormhole ID above is dead code.

---

## Appendix A: resolved crate graph for `velocity`

233 entries, default features, `cargo tree -e normal`. Regenerate with the first command in §7.

```
aead 0.5.2
aes 0.8.4
aes-gcm-siv 0.11.1
allocator-api2 0.2.21
anchor-attribute-access-control 1.0.2
anchor-attribute-account 1.0.2
anchor-attribute-constant 1.0.2
anchor-attribute-error 1.0.2
anchor-attribute-event 1.0.2
anchor-attribute-program 1.0.2
anchor-derive-accounts 1.0.2
anchor-derive-serde 1.0.2
anchor-derive-space 1.0.2
anchor-lang 1.0.2
anchor-lang-idl 0.1.2
anchor-lang-idl-spec 0.1.0
anchor-spl 1.0.2
anchor-syn 1.0.2
anyhow 1.0.102
arrayref 0.3.9
arrayvec 0.7.6
base16ct 0.2.0
base64 0.13.1
base64 0.21.7
base64 0.22.1
bincode 1.3.3
bitflags 2.11.1
blake3 1.8.5
block-buffer 0.10.4
borsh 1.6.1
borsh-derive 1.6.1
bs58 0.5.1
bv 0.11.1
bytemuck 1.25.0
bytemuck_derive 1.10.2
byteorder 1.5.0
cfg-if 1.0.4
chrono 0.4.44
cipher 0.4.4
const-crypto 0.3.0
const-oid 0.9.6
constant_time_eq 0.4.2
convert_case 0.4.0
core-foundation-sys 0.8.7
cpufeatures 0.2.17
crunchy 0.2.4
crypto-bigint 0.5.5
crypto-common 0.1.7
ctr 0.9.2
curve25519-dalek 4.1.3
der 0.7.10
derivation-path 0.2.0
derive_more 0.99.20
digest 0.10.7
ecdsa 0.16.9
either 1.15.0
elliptic-curve 0.13.8
enumflags2 0.6.4
enumflags2_derive 0.6.4
equivalent 1.0.2
ff 0.13.1
five8 1.0.0
five8_const 1.0.0
five8_core 1.0.0
fnv 1.0.7
foldhash 0.1.5
generic-array 0.14.7
getrandom 0.2.17
group 0.13.0
hashbrown 0.15.2
hashbrown 0.17.0
heck 0.3.3
heck 0.5.0
hex 0.4.3
hmac 0.12.1
humantime 2.3.0
humantime-serde 1.1.1
iana-time-zone 0.1.65
indexmap 2.14.0
inout 0.1.4
itertools 0.12.1
itertools 0.13.0
itoa 1.0.18
k256 0.13.4
keccak 0.1.6
keccak-const 0.2.0
lazy_static 1.5.0
libc 0.2.185
log 0.4.29
memchr 2.8.0
memoffset 0.9.1
merlin 3.0.0
num-bigint 0.4.6
num-derive 0.4.2
num-integer 0.1.46
num-traits 0.2.19
num_enum 0.7.6
num_enum_derive 0.7.6
once_cell 1.21.4
opaque-debug 0.3.1
pbkdf2 0.11.0
percent-encoding 2.3.2
pkcs8 0.10.2
polyval 0.6.2
ppv-lite86 0.2.21
proc-macro-crate 3.4.0
proc-macro2 1.0.106
pyth-client 0.2.2
pyth_lazer 0.1.0 (local: programs/pyth-lazer)
qstring 0.7.2
quote 1.0.45
rand 0.8.5
rand_chacha 0.3.1
rand_core 0.6.4
rfc6979 0.4.0
rust_decimal 1.41.0
rustversion 1.0.22
ryu 1.0.23
sec1 0.7.3
serde 1.0.228
serde_bytes 0.11.19
serde_core 1.0.228
serde_derive 1.0.228
serde_json 1.0.143
sha2 0.10.9
sha2-const-stable 0.1.0
sha3 0.10.8
signature 2.2.0
solana-account-info 3.1.1
solana-address 1.1.0
solana-address 2.6.0
solana-address-lookup-table-interface 3.1.0
solana-atomic-u64 3.0.1
solana-big-mod-exp 3.0.0
solana-blake3-hasher 3.1.0
solana-borsh 3.0.2
solana-clock 3.0.1
solana-cpi 3.1.0
solana-curve25519 3.1.13
solana-define-syscall 3.0.0
solana-define-syscall 4.0.1
solana-derivation-path 3.0.0
solana-epoch-rewards 3.0.1
solana-epoch-schedule 3.1.0
solana-epoch-stake 3.0.1
solana-example-mocks 3.0.0
solana-feature-gate-interface 3.1.0
solana-fee-calculator 3.2.0
solana-hash 3.1.0
solana-hash 4.3.0
solana-instruction 3.4.0
solana-instruction-error 2.3.0
solana-instructions-sysvar 3.0.0
solana-invoke 0.5.0
solana-keccak-hasher 3.1.0
solana-last-restart-slot 3.0.0
solana-loader-v3-interface 6.1.1
solana-message 3.1.0
solana-msg 3.1.0
solana-native-token 3.0.0
solana-nonce 3.2.0
solana-nullable 1.1.0
solana-program 3.0.0
solana-program-entrypoint 3.1.1
solana-program-error 3.0.1
solana-program-memory 3.1.0
solana-program-option 3.1.0
solana-program-pack 3.1.0
solana-pubkey 3.0.0
solana-pubkey 4.2.0
solana-rent 3.1.0
solana-sanitize 3.0.1
solana-sdk-ids 3.1.0
solana-sdk-macro 3.0.1
solana-secp256k1-recover 3.1.1
solana-security-txt 1.1.2
solana-seed-derivable 3.0.0
solana-seed-phrase 3.0.0
solana-serde-varint 3.0.1
solana-serialize-utils 3.1.1
solana-sha256-hasher 3.1.0
solana-short-vec 3.2.0
solana-signature 3.4.0
solana-signer 3.0.0
solana-slot-hashes 3.0.1
solana-slot-history 3.0.0
solana-stable-layout 3.0.1
solana-stake-interface 2.0.2
solana-system-interface 2.0.0
solana-system-interface 3.2.0
solana-sysvar 3.1.1
solana-sysvar-id 3.1.0
solana-transaction-error 3.2.0
solana-zero-copy 1.0.1
solana-zk-sdk 4.0.0
spki 0.7.3
spl-associated-token-account-interface 2.0.0
spl-discriminator 0.5.2
spl-discriminator-derive 0.2.0
spl-discriminator-syn 0.2.1
spl-pod 0.7.3
spl-token-2022-interface 2.1.0
spl-token-confidential-transfer-proof-extraction 0.5.1
spl-token-confidential-transfer-proof-generation 0.5.1
spl-token-group-interface 0.7.2
spl-token-interface 2.0.0
spl-token-metadata-interface 0.8.0
spl-type-length-value 0.9.1
static_assertions 1.1.0
strum 0.27.2
strum_macros 0.27.2
subtle 2.6.1
syn 1.0.109
syn 2.0.117
thiserror 1.0.69
thiserror 2.0.18
thiserror-impl 1.0.69
thiserror-impl 2.0.18
toml_datetime 0.7.5+spec-1.1.0
toml_edit 0.23.10+spec-1.0.0
toml_parser 1.1.0+spec-1.1.0
typenum 1.19.0
uint 0.9.5
unicode-ident 1.0.24
unicode-segmentation 1.13.2
universal-hash 0.5.1
uriparse 0.6.4
velocity 2.165.0 (local: programs/velocity)
winnow 0.7.15
winnow 1.0.1
zerocopy 0.8.48
zeroize 1.8.2
zeroize_derive 1.4.3
```
