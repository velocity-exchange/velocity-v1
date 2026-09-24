# deploy-scripts

Devnet deployment scripts for the velocity program. The devnet quote token is dUSDT, a
velocity-controlled SPL mint created in Phase 0 and distributed through the `token_faucet` program.
Internal env vars and identifiers still use `USDT` for brevity (`USDT_MINT`, `usdtMint`). The onchain
ticker and spot market name are both `dUSDT`.

The devnet program id comes from `[programs.devnet].velocity` in `Anchor.toml`. That value and the
`declare_id!` in `programs/velocity/src/lib.rs` are the source of truth, and Anchor enforces that
they match. Override with `VELOCITY_DEVNET_PROGRAM_ID=…` only for one-off testing.

## Release CLI (`bun run release`)

[`release.sh`](./release.sh) gives one verb per release step, in release order. It reads the same
files CI reads, which are `programs/*/Cargo.toml`, `packages/*/package.json`, `docker-info.json`, the
tag conventions in the workflows, and the infra gitops manifests. Each verb fires exactly one thing,
either a git push, a `gh workflow run`, or an infra `yarn deploy`, then prints the URL. It does not
wait on CI, verify buffers, approve Squads proposals or merge PRs. GitHub, `verify-buffer.sh`, Squads
and ArgoCD do those, and `status` shows where each one stands.

Every verb is read-only by default and prints what `--execute` would do. It needs nothing beyond
`git`, `gh` and `jq`.

```bash
bun run release                       # status: programs, npm, docker, gitops pins, and the one command to run next
bun run release bump    [prog] [ver]  # release/<prog>-<ver> branch: Cargo.toml + lockfiles + IDLs, commit (your key), push, PR link
bun run release devnet  [prog]        # gh workflow run manual-devnet-deploy.yaml            [--branch <ref>]
bun run release npm     [pkg...]      # push npm-<pkg>-v<version> for every untagged package version, one push per tag
bun run release docker  [app...]      # push docker-<app>-v<next patch> for images with commits since their tag, one push per tag
bun run release infra   <stage...>    # infra-v3 `yarn deploy a,b,c <stage>` for every image whose pin is behind: one PR
bun run release mainnet [prog]        # push program-<prog>-v<Cargo version> at origin/master
```

Flags: `--execute`, `--infra <path>` (or `VELOCITY_INFRA_DIR`), `--branch <ref>` (devnet only),
`--no-fetch`, `--no-color`, `-h`. Programs default to `velocity`. On devnet only, `token_faucet` is
the other choice.

`status` also warns about several conditions that are easy to miss. It flags a Cargo version that is
not ahead of the last `program-*` tag while the program has new commits, because the tag would
collide. It flags velocity diffs since the last tag that touch the fee schedule (run the fee-schedule
admin call before the upgrade, described below), the error enum, or the IDL. It flags an open
changesets "Version Packages" PR, and gitops pins older than the latest image. `devnet` and `mainnet`
print the same warnings before firing. Docker versions come from the tag history rather than from
`apps/*/package.json`. The bump commit runs in your terminal under your git identity, and it runs a
`gpg --clearsign` first so the passphrase prompt is not buried under other output.

### Release order

Devnet must run the exact bytes mainnet will get, so the version bump comes first and both clusters
deploy the same sha.

```bash
export VELOCITY_INFRA_DIR=~/work/velocity/infrastructure-v3
aws sso login --sso-session velocity           # the infra step resolves ECR digests

bun run release                                # where are we; the last line is the next command

# 1. version bump PR
bun run release bump --execute                 # release/velocity-X.Y.0 branch, PR link; merge it

# 2. program to devnet (that sha)
bun run release devnet --execute               # dispatches CI; ~15 min
bun run verify-buffer velocity --devnet        # when the run is green: hashes must match
#    approve + execute the proposal in the devnet Squads

# 3. packages and images (after the changesets "Version Packages" PR is merged)
bun run release npm --execute                  # npm-sdk-vX.Y.Z etc.
bun run release docker --execute               # docker-<app>-v<next> for every image with changes

# 4. devnet bots
bun run release infra master --execute         # one infra-v3 PR; merge it, ArgoCD rolls
#    soak on devnet

# 5. program to mainnet (same sha)
bun run release mainnet --execute              # pushes program-velocity-vX.Y.0
bun run verify-buffer velocity                 # when the run is green
#    approve + execute the proposal in the mainnet Squads

# 6. prod bots
bun run release infra mainnet-beta --execute   # one infra-v3 PR; merge it
#    then open the infra-v3 release PR from master into mainnet-beta
#    (prod ArgoCD tracks the mainnet-beta branch)

bun run release                                # everything green: "nothing to release"
```

Steps 2 and 3 are independent and can run in parallel. Any admin instruction the upgrade needs, such
as a fee schedule change, goes before the Squads execution on each cluster. `status` flags the known
cases.

## Program upgrades via CI (preferred)

Program upgrades to mainnet and devnet are gated through a Squads multisig and proposed by GitHub
Actions. The scripts in this directory remain for emergency and direct deploys against the devnet
upgrade keypair.

| Target | Trigger | Workflow |
| --- | --- | --- |
| mainnet | Push tag `program-<name>-v<version>` where `<name>` is the program lib name, `velocity` (for example `program-velocity-v2.163.0`) | [`.github/workflows/release-program.yaml`](../.github/workflows/release-program.yaml) |
| devnet | Run **Manual Devnet Program Deploy** from the Actions tab, picking program and branch | [`.github/workflows/manual-devnet-deploy.yaml`](../.github/workflows/manual-devnet-deploy.yaml) |

Both workflows do the same thing on different multisigs.

1. Build the program. `anchor idl build` produces the IDL JSON with no SBF compile, and
   `solana-verify build` produces a reproducible `.so` from a Docker image pinned in the workflow
   env. The devnet velocity build strips `mainnet-beta`, so the devnet-only instructions are compiled
   in and the production gates are off. The local
   [`build-program`](../.github/actions/build-program/) composite action does this, because the
   Solana Foundation reusable build cannot express `--skip-lint` or devnet's
   `--no-default-features`.

2. Stage the upgrade with
   [`solana-foundation/github-actions/prepare-squads-release`](https://github.com/solana-foundation/github-actions/tree/main/prepare-squads-release),
   pinned by commit SHA and wrapped by the local
   [`buffer-deploy`](../.github/actions/buffer-deploy/) action. It writes the `.so` to a BPF
   Upgradeable Loader buffer, which is resumable and re-sends only missing chunks on a retry. It
   writes the IDL JSON to a program-metadata buffer. It transfers both buffer authorities to the
   multisig vault PDA. On mainnet it also exports a `solana-verify` PDA transaction. Before any of
   that, `buffer-deploy` asserts that the program's canonical IDL metadata account already exists. It
   never creates that account, and it fails fast with instructions if the account is missing, so see
   [Initial deploy](#initial-deploy-create-the-idl-metadata-account) below. It then logs the program
   buffer address, the metadata buffer address, and the onchain buffer hash next to the local
   verifiable `.so` hash in the run summary, for multisig-side verification. Reproduce that hash with
   [`verify-buffer.sh`](#verifying-a-buffer-before-signing-the-squads-proposal).

3. Propose the Squads transaction with
   [`solana-foundation/squads-program-action`](https://github.com/solana-foundation/squads-program-action),
   which is official and SHA-pinned. Because the IDL metadata account already exists, this is a
   single vault transaction holding `SetData` from the IDL buffer (grown first only if the IDL
   changed by less than 10 KiB), the BPF Loader `Upgrade`, and on mainnet the `solana-verify` PDA
   instruction. One transaction, no batch. The proposal is not auto-executed. Multisig signers
   approve and execute it through the Squads UI.

   > CI only ever updates the IDL. The canonical metadata account can only be created by the
   > program's upgrade authority, so it is created once at initial program deploy, while the deployer
   > still holds upgrade authority and before authority is handed to the multisig. See
   > [Initial deploy](#initial-deploy-create-the-idl-metadata-account). After that, every release is
   > a single-transaction `SetData` plus `Upgrade`.

### Required GitHub secrets

| Secret | Purpose |
| --- | --- |
| `MAINNET_RPC_ENDPOINT` / `DEVNET_RPC_ENDPOINT` | Solana RPC URLs. Use a private RPC for mainnet, because write-buffer needs roughly 1200 chunked writes. |
| `MAINNET_DEPLOYER_KEYPAIR` / `DEVNET_DEPLOYER_KEYPAIR` | Solana keypair as a raw `[..]` byte array. Pays buffer rent and signs the Squads proposal. Must be a multisig member with Voter permissions. |
| `MAINNET_MULTISIG` / `DEVNET_MULTISIG` | Squads multisig PDA. |
| `MAINNET_MULTISIG_VAULT` / `DEVNET_MULTISIG_VAULT` | The vault PDA owned by the multisig (Squads vault index 0). This is the onchain program upgrade authority and the IDL metadata authority. |

### Initial deploy: create the IDL metadata account

CI only updates the IDL and never creates the canonical metadata account, because creating one
requires the program's upgrade authority to sign. The program-metadata spec puts it plainly:
canonical metadata accounts are created by the program upgrade authority. After launch the upgrade
authority is the multisig vault, and creating velocity's roughly 53 KB account through a vault CPI
would need a batched proposal. Instead, the canonical IDL account is created once by the deployer at
initial program deploy, while the deployer still holds the upgrade authority. No multisig and no
batch are involved, because the deployer sends the chunked writes directly.

The Anchor CLI does not do this. `anchor deploy` only deploys the program, and `anchor idl init`
targets the legacy onchain IDL account rather than the program-metadata account velocity's clients
resolve. Use the program-metadata CLI explicitly.

Run this once per cluster, against a private RPC, in the order given, and before transferring the
upgrade authority to the multisig. Mainnet is not deployed yet. Devnet's account already exists.

```bash
PROGRAM_ID=vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P
RPC=<private-rpc-url>
DEPLOYER=<deployer-keypair.json>          # must be the current program upgrade authority
VAULT=<MAINNET_MULTISIG_VAULT pubkey>

# 1. Deploy the program (deployer is the upgrade authority at this point).
solana program deploy target/deploy/velocity.so \
  --program-id <program-keypair.json> \
  --upgrade-authority "$DEPLOYER" -u "$RPC" --use-rpc

# 2. Build the mainnet IDL JSON (no SBF compile; default features = mainnet, so
#    devnet-only instructions are excluded, matching what CI's build-program emits).
anchor idl build --skip-lint -p velocity -o target/idl/velocity.json

# 3. Create the canonical IDL metadata account (deployer signs as upgrade authority).
npx @solana-program/program-metadata@0.5.1 create idl "$PROGRAM_ID" \
  target/idl/velocity.json --keypair "$DEPLOYER" --rpc "$RPC"

# 4. Delegate the metadata account to the multisig vault so CI can update it.
npx @solana-program/program-metadata@0.5.1 set-authority idl "$PROGRAM_ID" \
  --new-authority "$VAULT" --keypair "$DEPLOYER" --rpc "$RPC"

# 5. Hand the program upgrade authority to the multisig vault (LAST, after the
#    account exists; the vault remains able to update the canonical account as the
#    upgrade authority, and is also the delegated metadata authority from step 4).
solana program set-upgrade-authority "$PROGRAM_ID" \
  --new-upgrade-authority "$VAULT" -k "$DEPLOYER" -u "$RPC"
```

From then on, the tag-triggered and dispatch-triggered workflows above handle every release as a
single-transaction `SetData` plus `Upgrade`. If `buffer-deploy` fails with "Canonical IDL metadata
account … does not exist", this step was skipped.

### Cutting a mainnet release

```bash
# 1. Bump programs/velocity/Cargo.toml version
# 2. Land that on mainnet-beta
git checkout mainnet-beta
git pull
# 3. Tag it
git tag program-velocity-v2.163.0
git push origin program-velocity-v2.163.0
# 4. In the Actions tab, watch the "Release Program to Mainnet" run and wait for the Squads proposal
# 5. Sign + execute in the Squads UI
```

The `mainnet-beta` branch tracks what is live on mainnet, or about to be. `master` is active
development. The tag itself is the deploy trigger, so branch state does not gate the workflow. This
assumes the program and its canonical IDL account already exist on mainnet. For the very first
mainnet deploy, do [Initial deploy](#initial-deploy-create-the-idl-metadata-account) first.

### Upgrades that change the default fee schedule

`FeeStructure::perps_default()` runs only in `initialize`, so a program upgrade never rewrites the
fee structure stored in `State`. The hardcoded tier thresholds and the tier count in
`determine_perp_fee_tier` do change the moment the new program is live. If an upgrade changes the
default schedule, meaning tier rates, thresholds, or tier count, the stored tiers and the code's
thresholds disagree until an `update_perp_fee_structure` call lands. Fees are wrong in both
directions during that window. Treat the admin call as part of the upgrade itself. Stage it alongside
the Squads proposal, or send it immediately after the swap on devnet, rather than as a follow-up.

### Verifying a buffer before signing the Squads proposal

Before approving an upgrade in the Squads UI, confirm the staged buffer is what the source compiles
to. Do not trust the hash CI printed. The CI `buffer-deploy` step logs the program buffer address and
its hash, and `verify-buffer.sh` reproduces the hash from source and compares the two.

```bash
# Usual case: find the newest deploy run for this program yourself, build the
# devnet flavor, and compare against the buffer that run staged:
deploy-scripts/verify-buffer.sh velocity --devnet --rpc "$SOLANA_RPC"

# Or point it at a specific run:
deploy-scripts/verify-buffer.sh velocity \
  https://github.com/velocity-exchange/velocity-v1/actions/runs/<id>/job/<id> \
  --devnet --rpc "$SOLANA_RPC"

# Or check a known buffer directly, reusing an already-built .so:
deploy-scripts/verify-buffer.sh velocity --buffer <bufferPubkey> --rpc "$SOLANA_RPC" --skip-build
```

With neither a run URL nor `--buffer`, it scans the newest successful runs of `release-program.yaml`,
or of `manual-devnet-deploy.yaml` when `--devnet` is passed. It takes the first run whose deploy
summary staged `<program>`, so there is no buffer address to copy by hand. The run it picked is
printed in the result block.

It exits non-zero on a mismatch. It needs `solana-verify`, an authenticated `gh`, and the solana CLI
on `PATH`. Drop `--devnet` for a mainnet build.

The docker build output stays out of the terminal. Each step prints one progress line, and a failing
step prints the last 30 lines of what it produced. Add `--verbose` to stream the build inline,
`--no-color` (or `NO_COLOR=1`) for plain output, and `-h` for the full flag list.

---

## Runbook

1. Build both programs, using an x86_64 toolchain as described in the root `CLAUDE.md`.

   ```
   bash deploy-scripts/build-devnet.sh
   ```

   This builds `velocity` (no default features, no `mainnet-beta` gate, devnet `declare_id!`) and
   `token_faucet`, which distributes devnet dUSDT. The deploy scripts read the devnet program id from
   `Anchor.toml`, so you do not need to set `VELOCITY_DEVNET_PROGRAM_ID` unless you are overriding it
   for one-off testing.

2. Deploy. For a first deploy of fresh programs there are two routes.

   - Vanity program id plus a named buffer, recommended for large velocity.so uploads. Build with
     `bash deploy-scripts/build-devnet.sh`, then save your program keypair JSON under
     `deploy-scripts/out/` (gitignored). Its pubkey must match `[programs.devnet].velocity` in
     `Anchor.toml`. If you only have a recovery phrase, recover once:

     ```
     solana-keygen recover ASK -o deploy-scripts/out/velocity-program-devnet.json --skip-seed-phrase-validation
     ```

     Paste your phrase when prompted. Pass `--skip-seed-phrase-validation` if the words are not on
     the BIP39 English list. Then create the onchain buffer and deploy from it:

     ```
     export VELOCITY_DEVNET_UPGRADE_KEYPAIR=/path/to/admin-or-buffer-authority.json
     export PROGRAM_KEYPAIR=$PWD/deploy-scripts/out/velocity-program-devnet.json
     bash deploy-scripts/write-buffer-devnet.sh
     BUFFER_ACCOUNT_KEYPAIR=$PWD/deploy-scripts/out/velocity-so-write-buffer-keypair.json \
       PROGRAM_KEYPAIR=$PROGRAM_KEYPAIR bash deploy-scripts/deploy-from-buffer-devnet.sh
     ```

     If your vanity run only printed a short "seed" (for example `6IPs6rIASB0S38TO`), treat it as the
     custom word or passphrase your tool uses along with the rest of its output. The recovered pubkey
     must equal `[programs.devnet].velocity` from `Anchor.toml`. Confirm with `solana-keygen pubkey`
     on the recovered JSON. The deploy scripts reject a `PROGRAM_KEYPAIR` whose pubkey does not
     match. Prefer a private devnet RPC through `SOLANA_RPC` or `RPC_URL` so `write-buffer` does not
     hit rate limits.

   - Alternatively, run `anchor deploy --program-name velocity` and
     `anchor deploy --program-name token_faucet` against devnet with `PROGRAM_KEYPAIR` or
     `--program-keypair`.

   For subsequent upgrades on the same program id, run `bash deploy-scripts/deploy-devnet.sh`. The
   script reads the program id from `Anchor.toml`. Set `VELOCITY_DEVNET_UPGRADE_KEYPAIR` to the path
   of the upgrade authority keypair, or use the legacy pair `SOLANA_PATH` plus `DEVNET_ADMIN`.
   Override `VELOCITY_DEVNET_PROGRAM_ID=…` only for one-off testing against a non-canonical id.

3. Sync the IDL into the SDK so the init script sees current instruction shapes.

   ```
   bun run program:idl
   ```

   That regenerates `packages/sdk/src/idl/velocity.json` and `packages/sdk/src/idl/velocity.ts` with
   `--no-default-features --features no-entrypoint,anchor-test`, which is the flavor that keeps the
   devnet-only instructions in the IDL. Never hand-edit either file.

4. Initialize onchain state. One pass, idempotent, covering phases 0, K, A, B, C, C+, C2, D, D2, E
   and F.

   ```
   DEVNET_ADMIN=/path/to/admin.json \
   SOL_LAZER_FEED_ID=<u32 feed id> \
   PYTH_LAZER_TOKEN=<pyth lazer relay token> \
   bash deploy-scripts/init-devnet.sh
   ```

   Phase 0 creates a fresh 6-decimal dUSDT SPL mint, pre-mints `USDT_INITIAL_SUPPLY` (default 10M) to
   the admin ATA, then initializes the `token_faucet` for that mint. It transfers mint authority to
   the faucet PDA so anyone can call `mint_to_user` for devnet dUSDT. The mint keypair is saved to
   `deploy-scripts/out/usdt-mint.json`, overridable with `USDT_MINT_KEYPAIR`, and the resolved mint
   pubkey is persisted to the receipt. Re-runs reuse the same mint. To skip mint creation and reuse
   an existing mint, set `USDT_MINT=<pubkey>`.

   Phase C+ subscribes to Pyth Lazer over WSS and posts an initial signed price update for both the
   SOL and USDT feeds. This is required because phase C2 (SOL spot) and phase D (SOL-PERP) call
   `get_oracle_price` at init, and so does phase E. Phase D2 then repegs SOL-PERP to the oracle
   price, because phase D seeds the AMM with placeholder reserves that put the mark price at $1.
   Phase E runs `update_spot_market_oracle` to switch dUSDT from `QuoteAsset`, the init source the
   program mandates for spot[0], to `PythLazerStableCoin` pointing at the USDT lazer PDA. Phase F is
   optional and can be skipped with `SKIP_PHASE_F=1`. A minimal functional devnet deploy is complete
   after Phase E.

   The script writes a receipt to `deploy-scripts/out/devnet-deployment.json` holding every created
   PDA, the dUSDT mint, the token_faucet config PDA, and the transaction signatures.

5. Patch the SDK constants with values from the receipt. These ship as `PublicKey.default`
   placeholders until the deployment exists.

   | File | Field | Receipt value |
   | --- | --- | --- |
   | `packages/sdk/src/config.ts` | `configs.devnet.QUOTE_MINT_ADDRESS` | `usdtMint` |
   | `packages/sdk/src/constants/spotMarkets.ts` | `DevnetSpotMarkets[0].mint` | `usdtMint` |
   | `packages/sdk/src/constants/spotMarkets.ts` | `DevnetSpotMarkets[0].oracle` | `pythLazerOracles[<usdtFeedId>].pubkey` |
   | `packages/sdk/src/constants/spotMarkets.ts` | `DevnetSpotMarkets[1].oracle` | `pythLazerOracles[<solFeedId>].pubkey` |
   | `packages/sdk/src/constants/perpMarkets.ts` | `DevnetPerpMarkets[0].oracle` | `pythLazerOracles[<solFeedId>].pubkey` |

## Distributing devnet dUSDT to test wallets

After Phase 0 the `token_faucet` program owns the dUSDT mint authority. Any wallet can request tokens
by calling `token_faucet.mint_to_user(amount)` with its ATA. See
`packages/sdk/src/tokenFaucet.ts` for a TypeScript client. The receipt records the faucet program id,
the `faucet_config` PDA and the `mint_authority` PDA, so bots and scripts can wire up directly.

Phase K funds keeper wallets with dUSDT out of the admin balance, so keepers can stake the insurance
fund that the bid/ask-twap crank requires. Set `KEEPER_PUBKEYS` to run it.

## Env vars

Required:

- `DEVNET_ADMIN`, path to the admin keypair file. It becomes `State.admin` immutably, and it is the
  initial dUSDT mint authority until Phase 0 hands that to the faucet PDA.
- `SOL_LAZER_FEED_ID`, the Pyth Lazer u32 feed id for SOL/USD.
- `PYTH_LAZER_TOKEN`, the auth token for the Pyth Lazer relay. It is required because non-quote spot
  markets and perp markets call `get_oracle_price` at init, and so does `update_spot_market_oracle`
  in Phase E. Phase C+ subscribes to the relay and posts a signed price update before the dependent
  phases run.

Optional:

- `USDT_LAZER_FEED_ID`, the Pyth Lazer u32 feed id for USDT/USD (default `8`). The PythLazerOracle
  PDA for this feed becomes the dUSDT spot[0] oracle after Phase E.
- `PYTH_LAZER_ENDPOINTS`, comma-separated WSS endpoints (default
  `wss://pyth-lazer.dourolabs.app/v1/stream`).
- `PYTH_LAZER_WAIT_MS`, milliseconds to wait for the first signed price message before failing
  (default `30000`).
- `USDT_MINT`, reuse an existing dUSDT SPL mint (6 decimals) instead of creating one.
- `USDT_MINT_KEYPAIR`, path to the keypair for the mint to create (default
  `deploy-scripts/out/usdt-mint.json`). Use a vanity keypair if you want one.
- `USDT_INITIAL_SUPPLY`, whole-token amount pre-minted to admin before the faucet takes mint
  authority (default `10000000`).
- `KEEPER_PUBKEYS`, comma-separated keeper authority pubkeys to seed with dUSDT in Phase K. Unset
  skips the phase.
- `KEEPER_FUND_AMOUNT`, whole dUSDT sent to each keeper (default `1000`).
- `TOKEN_FAUCET_PROGRAM_ID`, override (default `V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB`).
- `RPC_URL` (default `https://api.devnet.solana.com`)
- `LP_POOL_ID` (default `1`; id `0` is the "not in a pool" sentinel)
- `LP_MAX_AUM` (default `1000000`, multiplied by `QUOTE_PRECISION`)
- `RECEIPT_PATH` (default `deploy-scripts/out/devnet-deployment.json`)
- `SKIP_PHASE_C2=1`, `SKIP_PHASE_D=1`, `SKIP_PHASE_E=1`, `SKIP_PHASE_F=1` bypass individual phases.
  Phase F (LP pool plus dUSDT constituent) is optional, so skip it freely. Phase E, the oracle
  switch, is required for a functional dUSDT spot[0], so only skip it during partial re-runs.
- `HOT_MM_ORACLE_CRANK`, `HOT_AMM_CRANK`, `HOT_AMM_SPREAD_ADJUST`, `HOT_FEATURE_FLAG`,
  `HOT_FEE_WITHDRAW`, `HOT_FUEL`, `HOT_LP_CACHE`, `HOT_LP_SETTLE`, `HOT_LP_SWAP`, `HOT_USER_FLAG`,
  `HOT_VAMM_QUOTE_MANAGEMENT`, `HOT_VAULT_DEPOSIT` override the default hot-wallet pubkey for each
  native-crank role set in Phase A.1b.
- `NON_INTERACTIVE=1` (or `YES=1`) skips every confirmation prompt, which is useful in CI.

By default the script pauses before pre-flight and before each phase, printing the resolved inputs
(mint, oracle, LP id, and so on) and waiting for `y` to continue. Pre-flight verifies that both the
velocity and token_faucet programs are deployed and executable, and that any caller-supplied
`USDT_MINT` is a real token mint, before any state is touched.

## What gets initialized

See `.claude/plans/velocity-devnet-deployment.md` for the authoritative plan. In summary:

Required phases, 0 through E:

- `0` dUSDT SPL mint (6 decimals) plus `token_faucet` initialized for that mint
- `K` fund keeper wallets with dUSDT (runs only when `KEEPER_PUBKEYS` is set)
- `A` global `State` (A.1), native-crank authorities and feature flags (A.1b), and `AmmCache` (A.2)
- `B` dUSDT spot market at index 0 (the program forces oracle source `QuoteAsset`)
- `C` Pyth Lazer SOL and USDT oracle PDAs, created empty
- `C+` post the initial Pyth Lazer signed price update for both feeds, in one transaction
- `C2` SOL spot market at index 1, using the SOL Pyth Lazer oracle
- `D` SOL-PERP at index 0, using the SOL Pyth Lazer oracle
- `D2` repeg SOL-PERP to the oracle price, since Phase D seeds placeholder reserves
- `E` switch the dUSDT spot market oracle to `PythLazerStableCoin` pointing at the USDT lazer PDA.
  This is required because the program forces `QuoteAsset` at init for spot[0]
  (`programs/velocity/src/instructions/admin.rs:287-298`), and the only route to
  `PythLazerStableCoin` is the post-init `update_spot_market_oracle` instruction.

Optional phase, skipped with `SKIP_PHASE_F=1`:

- `F` LP pool plus dUSDT constituent

Skipped by design, left for later:

- Additional spot markets beyond dUSDT and SOL (BTC, ETH, and so on)
- `initializeIfRebalanceConfig`, which needs at least 2 spot markets. That is now satisfied, so it
  can be enabled.
- Spot DEX fulfillment (OpenBook V2, Phoenix, Serum)
- User-invoked flows: `initializeInsuranceFundStake`, `initializeReferrerName`

## Verification

After the script finishes, confirm the program is live. The program id comes from `Anchor.toml`:

```
solana program show "$(sh -c '. deploy-scripts/_lib.sh; velocity_devnet_program_id')" --url devnet
```

Then inspect the receipt at `deploy-scripts/out/devnet-deployment.json` and spot-check individual
accounts with `solana account <pubkey> --url devnet`.

For an end-to-end smoke test, use a second wallet to call
`VelocityClient.initializeUserAccount()`, then `deposit(usdtAmount, 0)`, then
`placePerpOrder({ marketIndex: 0, ... })`, and watch for a keeper fill.

## Operational notes (learned on first deploy)

- Use a private RPC for `solana program` writes. The public `api.devnet.solana.com` rate-limits the
  roughly 5,000 chunked writes a velocity upgrade requires, since velocity.so is about 5 MB written
  in 1 KB chunks. It fails partway through with `Data writes to account failed: Custom error: Max
  retries exceeded` or `Blockhash expired. N retries remaining`, leaving a partial buffer on chain.
  Pass a private RPC through `--url` to `solana program …` directly, or set `SOLANA_RPC` or `RPC_URL`
  for the helper scripts, which is what `write-buffer-devnet.sh` and `deploy-from-buffer-devnet.sh`
  read. `anchor program upgrade --provider.cluster <url>` works for the wrapper, but it does not
  propagate the URL to the underlying `solana program deploy` subprocess, so also run
  `solana config set --url <url>` before invoking anchor. Use your own private endpoint, such as a
  Triton, rpcpool or Helius URL. Do not rely on the public devnet endpoint for uploads.

- `anchor upgrade` is deprecated. Anchor 1.0 renamed it to `anchor program upgrade`. Same flags, same
  `solana program deploy` underneath. `deploy-devnet.sh` uses the new form.

- Prefer the two-phase flow of `write-buffer-devnet.sh` followed by `deploy-from-buffer-devnet.sh`
  over `anchor program upgrade`, for any upload larger than a few hundred KB. The single-shot
  `anchor program upgrade`, and a bare `solana program deploy <file.so>`, create an anonymous
  internal buffer and then auto-close it on a fatal error to refund the rent. The next attempt has
  nothing to resume from and starts at chunk 0. The two-phase flow uses a named buffer keypair, so
  the onchain buffer survives across attempts and `write-buffer` resumes by re-sending only the
  chunks that have not landed. Use the helper scripts:

  ```
  export VELOCITY_DEVNET_UPGRADE_KEYPAIR=/path/to/upgrade-authority.json
  export SOLANA_RPC=https://<your-private-rpc-endpoint>/<token>
  bash deploy-scripts/write-buffer-devnet.sh     # re-run this until it exits clean
  BUFFER_ACCOUNT_KEYPAIR=deploy-scripts/out/velocity-so-write-buffer-keypair.json \
    bash deploy-scripts/deploy-from-buffer-devnet.sh
  ```

  The buffer keypair file is reused across `write-buffer-devnet.sh` invocations, and each pass closes
  more gaps until the buffer is whole.

- Symptom: `Failed to parse ELF file: invalid section header`, or `invalid account data for
  instruction`, when running the swap with `program deploy --buffer …`. The buffer is partial,
  because some chunk writes never landed even though `write-buffer` exited 0. The CLI's exit code is
  not a reliable signal that the buffer is complete, since a successful last-batch retry can mask
  earlier dropped writes. Fix it with:

  ```
  solana program show <BUFFER_PK> --url <rpc>     # compare Data Length to ls -l target/deploy/velocity.so
  bash deploy-scripts/write-buffer-devnet.sh      # re-run; resume fills missing chunks
  ```

  Two or three resume passes is normal. The buffer is whole when `solana program show` reports a Data
  Length close to the .so size, allowing for the roughly 45-byte header the BPF loader prepends.
  Adding `--with-compute-unit-price 1000` to the underlying `solana program write-buffer` invocation,
  or through the `write-buffer-devnet.sh` env, helps individual chunk writes win contention faster.

- Why the upload restarts from scratch in the wrapper flow but not in the helper-script flow.
  `anchor program upgrade` and `solana program deploy <file>` invent a new buffer pubkey per
  invocation and never expose it, and on a partial failure the recent CLI tears the buffer down to
  refund the rent. The next invocation has no buffer to resume into. The helper scripts keep the
  buffer keypair file on disk, so the next invocation finds the same partial buffer on chain and
  writes only the gaps.

- If a buffer is orphaned, reclaim the SOL. Each abandoned buffer locks roughly 38 SOL of devnet rent
  for a velocity-sized buffer.

  ```
  # List buffers under each candidate authority (CLI default keypair vs. upgrade authority)
  solana program show --buffers --url <rpc>
  solana program show --buffers --url <rpc> \
    --buffer-authority $(solana-keygen pubkey "$VELOCITY_DEVNET_UPGRADE_KEYPAIR")

  # Close one
  solana program close <BUFFER_PK> --url <rpc> \
    --recipient $(solana-keygen pubkey "$VELOCITY_DEVNET_UPGRADE_KEYPAIR") \
    --buffer-authority "$VELOCITY_DEVNET_UPGRADE_KEYPAIR"

  # Close all buffers under one authority in one shot
  solana program close --buffers --url <rpc> \
    --recipient $(solana-keygen pubkey "$VELOCITY_DEVNET_UPGRADE_KEYPAIR") \
    --buffer-authority "$VELOCITY_DEVNET_UPGRADE_KEYPAIR"
  ```

  `--buffer-authority` must point at the keypair file, which is the signer, not just the pubkey. If
  `--buffers` finds nothing under either candidate authority but the balances look intact, the CLI
  already auto-closed and refunded, so no action is needed.

- `anchor build` and the velocity keypair mismatch. The checked-in
  `target/deploy/velocity-keypair.json` is a placeholder, so `anchor build` fails with "Program ID
  mismatch" on a clean checkout. Pass `--ignore-keys`. The deployed program id is hard-coded in
  source and the local keypair is unused for an upgrade.

- `bun` and strict type-only re-exports. `bun run deploy-scripts/init-devnet.ts` fails if the SDK
  re-exports a type without the `type` keyword, for example `export { PythLazerPriceFeedArray }`.
  This was fixed in `packages/sdk/src/index.ts` and `packages/sdk/src/pyth/index.ts`. Watch for it
  when adding new SDK exports.

- Pyth Lazer messages must include `feedUpdateTimestamp`. The onchain `post_pyth_lazer_oracle_update`
  instruction silently skips updates whose payload lacks `FeedUpdateTimestamp`
  (`programs/velocity/src/instructions/pyth_lazer_oracle.rs:99-110`). The transaction returns Ok with
  no onchain write, and the next phase fails with `Unable to read oracle price`. Phase C+ subscribes
  with `feedUpdateTimestamp` plus `bestBid`/`bestAskPrice` (used for confidence) and `exponent`. Do
  not strip these properties.

- The dUSDT oracle is a two-step init. `handle_initialize_spot_market`
  (`programs/velocity/src/instructions/admin.rs:287-298`) requires the quote spot market to use
  `OracleSource::QuoteAsset` with `oracle = Pubkey::default()`. Switching to `PythLazerStableCoin`
  afterwards happens in Phase E through `update_spot_market_oracle`, which itself reads the new
  oracle (`programs/velocity/src/instructions/admin.rs:1056-1061`), so the USDT lazer PDA must
  already hold a posted price. That is why Phase C+ runs before Phase E and must succeed.
