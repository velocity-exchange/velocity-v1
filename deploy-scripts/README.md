# deploy-scripts

Devnet deployment scripts for the velocity program. The devnet quote token is **dUSDT**, a
velocity-controlled SPL mint created in Phase 0 and distributed by the `token_faucet` program.
Internal environment variables and identifiers still use `USDT`, such as `USDT_MINT` and `usdtMint`.
The on-chain ticker and spot market name is `dUSDT`.

The devnet program id comes from `[programs.devnet].velocity` in `Anchor.toml`. That value and the
`declare_id!` in `programs/velocity/src/lib.rs` are the source of truth, and Anchor enforces that
they match. Override with `VELOCITY_DEVNET_PROGRAM_ID=…` only for one-off testing.

## Release CLI (`bun run release`)

[`release.sh`](./release.sh) is one verb per release step, in release order. It reads the files CI
reads: `programs/*/Cargo.toml`, `packages/*/package.json`, `docker-info.json`, the workflows' tag
conventions, and the infra gitops manifests. Each verb fires exactly one thing, a git push, a
`gh workflow run` or an infra `yarn deploy`, and then prints the URL. It does not wait on CI, verify
buffers, approve Squads or merge PRs. GitHub, `verify-buffer.sh`, Squads and ArgoCD do those, and
`status` shows where each one stands.

Every verb is **read-only by default** and prints what `--execute` would do. It needs only `git`,
`gh` and `jq`.

```bash
bun run release                       # status: programs, npm, docker, gitops pins, and the one command to run next
bun run release bump    [prog] [ver]  # release/<prog>-<ver> branch: Cargo.toml + lockfiles + IDLs, commit (your key), push, PR link
bun run release devnet  [prog]        # gh workflow run manual-devnet-deploy.yaml            [--branch <ref>]
bun run release npm     [pkg...]      # push npm-<pkg>-v<version> for every untagged package version, one push per tag
bun run release docker  [app...]      # push docker-<app>-v<next patch> for images with commits since their tag, one push per tag
bun run release infra   <stage...>    # infra-v3 `yarn deploy a,b,c <stage>` for every image whose pin is behind: one PR
bun run release mainnet [prog]        # push program-<prog>-v<Cargo version> at origin/master
```

Flags: `--execute`, `--infra <path>` (or `VELOCITY_INFRA_DIR`), `--branch <ref>` (devnet),
`--no-fetch`, `--no-color`. Programs default to `velocity`. On devnet only, `token_faucet` is the
other choice.

`status` also warns about the things a human forgets:

- a Cargo version that is not ahead of the last `program-*` tag while the program has new commits,
  because the tag would collide;
- velocity diffs since the last tag that touch the fee schedule, the error enum, or the IDL. Run
  `fees set-schedule` before a fee schedule upgrade. See [Upgrades that change the default fee
  schedule](#upgrades-that-change-the-default-fee-schedule);
- an open changesets "Version Packages" PR;
- gitops pins older than the latest image.

`devnet` and `mainnet` print the same warnings before they fire. Docker versions come from the tag
history, not from `apps/*/package.json`. The bump commit runs in your terminal with your git
identity. It runs a `gpg --clearsign` first, so the passphrase prompt is not buried under other
output.

### Release order

Devnet must run the exact bytes mainnet will get, so the version bump comes first and both clusters deploy the same sha.

```bash
export VELOCITY_INFRA_DIR=~/work/velocity/infrastructure-v3
aws sso login --sso-session velocity           # the infra step resolves ECR digests

bun run release                                # current state; the last line is the next command

# 1. version bump PR
bun run release bump --execute                 # release/velocity-X.Y.0 branch and a PR link; merge it

# 2. program to devnet (that sha)
bun run release devnet --execute               # dispatches CI; about 15 min
bun run verify-buffer velocity --devnet        # when the run is green: the hashes must match
#    approve and execute the proposal in the devnet Squads

# 3. packages and images (after the changesets "Version Packages" PR is merged)
bun run release npm --execute                  # npm-sdk-vX.Y.Z etc.
bun run release docker --execute               # docker-<app>-v<next> for every image with changes

# 4. devnet bots
bun run release infra master --execute         # one infra-v3 PR; merge it and ArgoCD rolls
#    soak on devnet

# 5. program to mainnet (same sha)
bun run release mainnet --execute              # pushes program-velocity-vX.Y.0
bun run verify-buffer velocity                 # when the run is green
#    approve and execute the proposal in the mainnet Squads

# 6. prod bots
bun run release infra mainnet-beta --execute   # one infra-v3 PR; merge it
#    then the infra-v3 master to mainnet-beta release PR (prod ArgoCD tracks that branch)

bun run release                                # everything green: "nothing to release"
```

Steps 2 and 3 are independent and can run in parallel. Any admin instruction the upgrade needs, such
as a fee schedule change, goes before the Squads execution on each cluster. `status` flags the known
cases.

## Program upgrades via CI (preferred)

Program upgrades to **mainnet** and **devnet** are gated through a Squads multisig and proposed by
GitHub Actions. The scripts in this directory stay for emergency and direct deploys against the
devnet upgrade keypair.

| Target | Trigger | Workflow |
| --- | --- | --- |
| **mainnet** | Push tag `program-<name>-<version>` where `<name>` is the program lib name: `velocity` (e.g. `program-velocity-2.163.0`) | [`.github/workflows/release-program.yaml`](../.github/workflows/release-program.yaml) |
| **devnet** | Run **Manual Devnet Program Deploy** from the Actions tab (pick program + branch) | [`.github/workflows/manual-devnet-deploy.yaml`](../.github/workflows/manual-devnet-deploy.yaml) |

Both workflows do the same thing on different multisigs:

1. Build the program. `anchor idl build` produces the IDL JSON with no SBF compile, and
   `solana-verify build` produces a reproducible `.so` from a Docker image pinned in the workflow
   env. Devnet velocity strips `mainnet-beta`, so the devnet-only instructions are compiled in and
   the production gates are off. The local [`build-program`](../.github/actions/build-program/)
   composite action does this. The Solana Foundation reusable build cannot express `--skip-lint` or
   devnet's `--no-default-features`, so the build stays in-house.
2. Stage the upgrade with [`solana-foundation/github-actions/prepare-squads-release`](https://github.com/solana-foundation/github-actions/tree/main/prepare-squads-release),
   pinned by commit SHA and wrapped by the local
   [`buffer-deploy`](../.github/actions/buffer-deploy/) action. It writes the `.so` to a BPF
   Upgradeable Loader buffer, resends only the missing chunks on a retry, writes the IDL JSON to a
   program-metadata buffer, and transfers both buffer authorities to the multisig vault PDA. On
   mainnet it also exports a `solana-verify` PDA transaction. `buffer-deploy` first asserts that the
   program's **canonical IDL metadata account already exists**. It never creates that account (see
   [Initial deploy](#initial-deploy-create-the-idl-metadata-account) below) and fails fast with
   instructions when the account is absent. It then logs the program and metadata buffer addresses,
   and the on-chain buffer hash next to the local verifiable `.so` hash, in the run summary.
   Reproduce those hashes with [`verify-buffer.sh`](#verifying-a-buffer-before-signing-the-squads-proposal).
3. Propose the Squads transaction with [`solana-foundation/squads-program-action`](https://github.com/solana-foundation/squads-program-action),
   the official action, pinned by SHA. Because the IDL metadata account already exists, this is a
   single vault transaction: `SetData` from the IDL buffer, which grows the account first only when
   the IDL changed by less than 10 KiB, then the BPF Loader `Upgrade`, then the `solana-verify` PDA
   instruction on mainnet. One transaction, no batch. The proposal is **not** executed
   automatically. Multisig signers approve and execute it through the Squads UI.

   > CI only ever **updates** the IDL. Only the program's upgrade authority can **create** the
   > canonical metadata account, so it is created once at the initial program deploy, while the
   > deployer still holds the upgrade authority and before the authority is handed to the multisig.
   > See [Initial deploy](#initial-deploy-create-the-idl-metadata-account). After that every release
   > is a single-transaction `SetData`.

### Required GitHub secrets

| Secret | Purpose |
| --- | --- |
| `MAINNET_RPC_ENDPOINT` / `DEVNET_RPC_ENDPOINT` | Solana RPC URLs. Use a private RPC for mainnet, because write-buffer needs about 1,200 chunked writes. |
| `MAINNET_DEPLOYER_KEYPAIR` / `DEVNET_DEPLOYER_KEYPAIR` | Solana keypair as a raw `[..]` byte array. Pays the buffer rent and signs the Squads proposal. Must be a multisig member with Voter permissions. |
| `MAINNET_MULTISIG` / `DEVNET_MULTISIG` | Squads multisig PDA. |
| `MAINNET_MULTISIG_VAULT` / `DEVNET_MULTISIG_VAULT` | The vault PDA owned by the multisig (Squads "vault index 0"). This is the on-chain program upgrade authority and the IDL metadata authority. |

### Initial deploy: create the IDL metadata account

CI **only updates** the IDL. It never creates the canonical metadata account, because creating one
requires the program's **upgrade authority** to sign. The program-metadata documentation states that
canonical metadata accounts are created by the program upgrade authority. After launch the upgrade
authority is the multisig vault, and creating velocity's roughly 53 KB account through a vault CPI
would need a batched proposal. So **the canonical IDL account is created once, by the deployer, at
the initial program deploy, while the deployer still holds the upgrade authority**. There is no
multisig and no batch, because the deployer sends the chunked writes directly. The Anchor CLI does
**not** do this: `anchor deploy` only deploys the program, and `anchor idl init` targets the legacy
on-chain IDL account rather than the program-metadata account velocity's clients resolve. Use the
program-metadata CLI explicitly.

Run this once per cluster, against a **private RPC**, in the order below, and **before** you transfer
the upgrade authority to the multisig. Mainnet is not deployed yet, and the devnet account already
exists.

```bash
PROGRAM_ID=vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P
RPC=<private-rpc-url>
DEPLOYER=<deployer-keypair.json>          # must be the current program upgrade authority
VAULT=<MAINNET_MULTISIG_VAULT pubkey>

# 1. Deploy the program (deployer is the upgrade authority at this point).
solana program deploy target/deploy/velocity.so \
  --program-id <program-keypair.json> \
  --upgrade-authority "$DEPLOYER" -u "$RPC" --use-rpc

# 2. Build the mainnet IDL JSON. No SBF compile. Default features mean mainnet, so
#    devnet-only instructions are excluded, which matches what CI's build-program emits.
anchor idl build --skip-lint -p velocity -o target/idl/velocity.json

# 3. Create the canonical IDL metadata account (deployer signs as upgrade authority).
npx @solana-program/program-metadata@0.5.1 create idl "$PROGRAM_ID" \
  target/idl/velocity.json --keypair "$DEPLOYER" --rpc "$RPC"

# 4. Delegate the metadata account to the multisig vault so CI can update it.
npx @solana-program/program-metadata@0.5.1 set-authority idl "$PROGRAM_ID" \
  --new-authority "$VAULT" --keypair "$DEPLOYER" --rpc "$RPC"

# 5. Hand the program upgrade authority to the multisig vault. Do this last, after the
#    account exists. The vault can still update the canonical account as the upgrade
#    authority, and it is also the delegated metadata authority from step 4.
solana program set-upgrade-authority "$PROGRAM_ID" \
  --new-upgrade-authority "$VAULT" -k "$DEPLOYER" -u "$RPC"
```

From then on, the workflows above handle every release as a single-transaction `SetData` and
`Upgrade`. When `buffer-deploy` fails with "Canonical IDL metadata account … does not exist", this
step was skipped.

### Cutting a mainnet release

```bash
# 1. Bump programs/velocity/Cargo.toml version
# 2. Land that on mainnet-beta
git checkout mainnet-beta
git pull
# 3. Tag it
git tag program-velocity-2.163.0
git push origin program-velocity-2.163.0
# 4. Watch Actions, Release Program to Mainnet, and wait for the Squads proposal
# 5. Sign and execute in the Squads UI
```

The `mainnet-beta` branch tracks what is live on mainnet, or about to be. `master` is active
development. The tag is the deploy trigger, and branch state does not gate the workflow. This
assumes the program and its canonical IDL account already exist on mainnet. For the first mainnet
deploy, do [Initial deploy](#initial-deploy-create-the-idl-metadata-account) first.

### Upgrades that change the default fee schedule

`FeeStructure::perps_default()` runs only in `initialize`, so a program upgrade never rewrites the
fee structure stored in `State`. The hardcoded tier *thresholds* and tier count in
`determine_perp_fee_tier` do change the moment the new program is live. When an upgrade changes the
default schedule, meaning the tier rates, the thresholds, or the tier count, the stored tiers and
the code's thresholds disagree until an `update_perp_fee_structure` call lands. Fees are wrong in
both directions in that window. Treat the admin call as part of the upgrade. Stage it alongside the
Squads proposal, or send it immediately after the swap on devnet, rather than as a follow-up.

### Verifying a buffer before signing the Squads proposal

Before you approve an upgrade in the Squads UI, confirm that the staged buffer is what the source
compiles to. Do not trust the hash CI printed. The CI `buffer-deploy` step logs the program buffer
address and its hash. `verify-buffer.sh` reproduces the hash from source and compares the two:

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

With neither a run URL nor `--buffer`, the script scans the newest successful runs of
`release-program.yaml`, or of `manual-devnet-deploy.yaml` with `--devnet`, and takes the first one
whose deploy summary staged `<program>`. So there is no buffer address to copy by hand. The result
block names the run it picked.

The script exits non-zero on a mismatch. It needs `solana-verify`, an authenticated `gh`, and the
solana CLI on `PATH`. Drop `--devnet` for a mainnet build.

The docker build output stays out of the terminal. Each step prints one progress line, and a failing
step prints the last 30 lines of what it produced. Add `--verbose` to stream the build inline,
`--no-color` (or `NO_COLOR=1`) for plain output, and `-h` for the full flag list.

---

## Runbook

1. **Build** both programs with the x86_64 toolchain. See the root `CLAUDE.md`.
   ```
   bash deploy-scripts/build-devnet.sh
   ```
   This builds `velocity` with no default features, no mainnet-beta gate and the devnet
   `declare_id!`, and it builds `token_faucet`, which distributes devnet dUSDT. The deploy scripts
   read the devnet program id from `Anchor.toml`. Set `VELOCITY_DEVNET_PROGRAM_ID` only to override
   it for one-off testing.
2. **Deploy** for the first time, onto fresh programs:
   - **Vanity program id and buffer.** Use this for large `velocity.so` uploads. Build with
     `bash deploy-scripts/build-devnet.sh`, then save your **program keypair JSON**, whose pubkey
     must match `[programs.devnet].velocity` in `Anchor.toml`, under `deploy-scripts/out/`, which is
     gitignored. When you hold only a recovery phrase, recover the keypair once:
     ```
     solana-keygen recover ASK -o deploy-scripts/out/velocity-program-devnet.json --skip-seed-phrase-validation
     ```
     Paste the phrase when prompted. Pass `--skip-seed-phrase-validation` when the words are not on
     the BIP39 English list. Then create the on-chain buffer and deploy from it:
     ```
     export VELOCITY_DEVNET_UPGRADE_KEYPAIR=/path/to/admin-or-buffer-authority.json
     export PROGRAM_KEYPAIR=$PWD/deploy-scripts/out/velocity-program-devnet.json
     bash deploy-scripts/write-buffer-devnet.sh
     BUFFER_ACCOUNT_KEYPAIR=$PWD/deploy-scripts/out/velocity-so-write-buffer-keypair.json \
       PROGRAM_KEYPAIR=$PROGRAM_KEYPAIR bash deploy-scripts/deploy-from-buffer-devnet.sh
     ```
     When the vanity run printed only a short "seed", such as `6IPs6rIASB0S38TO`, treat it as the
     custom word or passphrase your tool uses with the rest of its output. The recovered pubkey must
     equal `[programs.devnet].velocity` from `Anchor.toml`. Confirm it with `solana-keygen pubkey`
     on the recovered JSON. The deploy scripts reject a `PROGRAM_KEYPAIR` that does not match.
     Use a **private devnet RPC** through `SOLANA_RPC` or `RPC_URL` so `write-buffer` does not hit
     rate limits.

   - **The alternative** is `anchor deploy --program-name velocity` and
     `anchor deploy --program-name token_faucet` against devnet, with `PROGRAM_KEYPAIR` or
     `--program-keypair`.

   For **subsequent upgrades** on the same program id, run `bash deploy-scripts/deploy-devnet.sh`.
   The script reads the program id from `Anchor.toml`. Set `VELOCITY_DEVNET_UPGRADE_KEYPAIR` to the
   upgrade authority keypair path, or the legacy `SOLANA_PATH` and `DEVNET_ADMIN`. Override
   `VELOCITY_DEVNET_PROGRAM_ID=…` only for one-off testing against a non-canonical id.
3. **Sync the IDL** into the SDK so the init script sees the current instruction shapes:
   ```
   bun run program:idl
   ```
4. **Initialize on-chain state.** One idempotent pass runs phases 0 and A through E, then the
   optional phases:
   ```
   DEVNET_ADMIN=/path/to/admin.json \
   SOL_LAZER_FEED_ID=<u32 feed id> \
   PYTH_LAZER_TOKEN=<pyth lazer relay token> \
   bash deploy-scripts/init-devnet.sh
   ```
   Phase 0 creates a fresh 6-decimal dUSDT SPL mint, pre-mints `USDT_INITIAL_SUPPLY` (10 million by
   default) to the admin ATA, then initializes the `token_faucet` for that mint. It transfers the
   mint authority to the faucet PDA, so anyone can call `mint_to_user` for devnet dUSDT. The mint
   keypair is saved to `deploy-scripts/out/usdt-mint.json`, which `USDT_MINT_KEYPAIR` overrides, and
   the resolved mint pubkey is written to the receipt. A re-run reuses the same mint. To skip mint
   creation and reuse an existing mint, set `USDT_MINT=<pubkey>`.

   Phase C+ subscribes to Pyth Lazer over WSS and posts an initial signed price update for both the
   SOL and USDT feeds. Phase C2 (SOL spot), phase D (SOL-PERP) and phase E all call
   `get_oracle_price` at init, so they need that update. Phase E runs `update_spot_market_oracle` to
   switch dUSDT from `QuoteAsset`, which the program forces as the init source for spot[0], to
   `PythLazerStableCoin` pointing at the USDT lazer PDA. A minimal working devnet deploy is complete
   after Phase E. **Phase F is optional** and `SKIP_PHASE_F=1` skips it.

   The run writes a receipt to `deploy-scripts/out/devnet-deployment.json` with every created PDA,
   the dUSDT mint, the token_faucet config PDA, and the transaction signatures.
5. **Patch the SDK constants** with the values from the receipt. They ship as `PublicKey.default`
   placeholders until the deployment exists:
   - `packages/sdk/src/config.ts`: set `configs.devnet.QUOTE_MINT_ADDRESS` to `usdtMint`
   - `packages/sdk/src/constants/spotMarkets.ts`: set `DevnetSpotMarkets[0].mint` to `usdtMint`,
     `DevnetSpotMarkets[0].oracle` to `pythLazerOracles[<usdtFeedId>].pubkey`, and
     `DevnetSpotMarkets[1].oracle` to `pythLazerOracles[<solFeedId>].pubkey`
   - `packages/sdk/src/constants/perpMarkets.ts`: set `DevnetPerpMarkets[0].oracle` to
     `pythLazerOracles[<solFeedId>].pubkey`

## Distributing devnet dUSDT to test wallets

After Phase 0 the `token_faucet` program owns the dUSDT mint authority. Any wallet can request
tokens by calling `token_faucet.mint_to_user(amount)` with its ATA. `packages/sdk/src/tokenFaucet.ts`
holds a TypeScript client. The receipt records the faucet program id, the `faucet_config` PDA and
the `mint_authority` PDA, so a bot or a script can wire itself up directly.

## Env vars

Required:
- `DEVNET_ADMIN`: path to the admin keypair file. It becomes `State.admin` **immutably**, and it is
  the initial dUSDT mint authority until Phase 0 hands that to the faucet PDA.
- `SOL_LAZER_FEED_ID`: the Pyth Lazer u32 feed id for SOL/USD.
- `PYTH_LAZER_TOKEN`: the auth token for the Pyth Lazer relay. Non-quote spot markets and perp
  markets call `get_oracle_price` at init, and `update_spot_market_oracle` in Phase E does too.
  Phase C+ subscribes to the relay and posts a signed price update before the dependent phases run.

Optional:
- `USDT_LAZER_FEED_ID`: the Pyth Lazer u32 feed id for USDT/USD. Default `8`. The PythLazerOracle
  PDA for this feed becomes the dUSDT spot[0] oracle after Phase E.
- `PYTH_LAZER_ENDPOINTS`: comma-separated WSS endpoints. Default
  `wss://pyth-lazer.dourolabs.app/v1/stream`.
- `PYTH_LAZER_WAIT_MS`: milliseconds to wait for the first signed price message before failing.
  Default `30000`.
- `USDT_MINT`: reuse an existing 6-decimal dUSDT SPL mint instead of creating one.
- `USDT_MINT_KEYPAIR`: path to the keypair for the mint to create. Default
  `deploy-scripts/out/usdt-mint.json`. A vanity keypair works here.
- `USDT_INITIAL_SUPPLY`: whole-token amount pre-minted to the admin before the faucet takes the mint
  authority. Default `10000000`.
- `TOKEN_FAUCET_PROGRAM_ID`: default `V4v1mQiAdLz4qwckEb45WqHYceYizoib39cDBHSWfaB`.
- `KEEPER_PUBKEYS`: comma-separated keeper authority pubkeys to seed with dUSDT in Phase K, so they
  can stake the insurance fund that the bid/ask-twap crank requires. Unset skips Phase K.
- `KEEPER_FUND_AMOUNT`: whole dUSDT to send each keeper. Default `1000`.
- `RPC_URL`: default `https://api.devnet.solana.com`.
- `LP_POOL_ID`: default `1`. Id `0` is the "not in a pool" sentinel.
- `LP_MAX_AUM`: default `1_000_000`, multiplied by `QUOTE_PRECISION`.
- `RECEIPT_PATH`: default `deploy-scripts/out/devnet-deployment.json`.
- `SKIP_PHASE_C2=1`, `SKIP_PHASE_D=1`, `SKIP_PHASE_E=1`, `SKIP_PHASE_F=1`: bypass one phase. **Phase
  F is optional** (LP pool and dUSDT constituent), so skip it freely. Phase E (the oracle switch) is
  **required** for a working dUSDT spot[0]. Skip it only during a partial re-run.
- `NON_INTERACTIVE=1` (or `YES=1`): skip every confirmation prompt. Use it in CI.

By default the script pauses before pre-flight and before each phase. It prints the resolved inputs,
such as the mint, the oracle and the LP id, and waits for `y` to continue. Pre-flight verifies that
both the velocity and token_faucet programs are deployed and executable, and that any
caller-supplied `USDT_MINT` is a real token mint, before it touches any state.

## What gets initialized

`.claude/plans/velocity-devnet-deployment.md` holds the full plan. The summary:

Required phases, 0 through E:

- **0**  dUSDT SPL mint (6 decimals) and `token_faucet` initialized for that mint
- **A**  global `State` and `AmmCache`
- **B**  dUSDT spot market at index 0, with the oracle source forced by the program to `QuoteAsset`
- **C**  Pyth Lazer SOL and USDT oracle PDAs, created empty
- **C+** the initial Pyth Lazer signed price update for both feeds, in one transaction
- **C2** SOL spot market at index 1, on the SOL Pyth Lazer oracle
- **D**  SOL-PERP at index 0, on the SOL Pyth Lazer oracle
- **D2** repeg SOL-PERP so the mark price is near the oracle price
- **E**  switch the dUSDT spot market oracle to `PythLazerStableCoin` pointing at the USDT lazer
  PDA. The program forces `QuoteAsset` at init for spot[0] in `handle_initialize_spot_market`, so
  the post-init `update_spot_market_oracle` instruction is the only path to `PythLazerStableCoin`.

Optional phases:

- **F**  LP pool and dUSDT constituent. `SKIP_PHASE_F=1` skips it.
- **K**  seed the keeper authorities in `KEEPER_PUBKEYS` with dUSDT. An unset `KEEPER_PUBKEYS` skips
  it.

Left out on purpose, for later:
- spot markets beyond dUSDT and SOL, such as BTC and ETH
- `initializeIfRebalanceConfig`, which needs two or more spot markets. That condition holds now, so
  the phase can be enabled.
- spot DEX fulfillment through OpenBook V2, Phoenix or Serum
- the user-invoked flows `initializeInsuranceFundStake` and `initializeReferrerName`

## Verification

After the script finishes, confirm the program is live. The program id comes from `Anchor.toml`:

```
solana program show "$(sh -c '. deploy-scripts/_lib.sh; velocity_devnet_program_id')" --url devnet
```

Then inspect the receipt and spot-check the accounts with `solana account <pubkey> --url devnet`.

For an end-to-end smoke test, use a second wallet to call `VelocityClient.initializeUserAccount()`,
then `deposit(usdtAmount, 0)`, then `placePerpOrder({ marketIndex: 0, ... })`, and watch for a
keeper fill.

## Operational notes

- **Use a private RPC for `solana program` writes.** A velocity upgrade needs about 5,000 chunked
  writes, because `velocity.so` is around 5 MB and each chunk is 1 KB. The public
  `api.devnet.solana.com` rate-limits them and fails partway through with
  `Data writes to account failed: Custom error: Max retries exceeded`, or
  `Blockhash expired. N retries remaining`, or both, which leaves a partial buffer on chain. Pass a
  private RPC through `--url` to `solana program …` directly, or set `SOLANA_RPC` or `RPC_URL` for
  the helper scripts, which `write-buffer-devnet.sh` and `deploy-from-buffer-devnet.sh` both read.
  `anchor program upgrade --provider.cluster <url>` works for the wrapper, but it does **not** pass
  the URL to the underlying `solana program deploy` subprocess. Run `solana config set --url <url>`
  before you invoke anchor. Use your own private RPC endpoint, such as a Triton, rpcpool or Helius
  URL. Do not use the public devnet endpoint for uploads.

- **`anchor upgrade` is deprecated. Anchor 1.0 calls it `anchor program upgrade`.** Same flags, same
  `solana program deploy` underneath. `deploy-devnet.sh` uses the new form.

- **Prefer the two-phase `write-buffer` then `deploy-from-buffer` flow over
  `anchor program upgrade`** for any upload larger than a few hundred KB. The single-shot
  `anchor program upgrade`, and a bare `solana program deploy <file.so>`, create an *anonymous*
  internal buffer and close it on a fatal error to refund the rent. The next attempt has nothing to
  resume from and starts at chunk 0 again. The two-phase flow uses a **named buffer keypair**, so
  the on-chain buffer survives across attempts and `write-buffer` resumes by resending only the
  chunks that did not land. Use the helper scripts:
  ```
  export VELOCITY_DEVNET_UPGRADE_KEYPAIR=/path/to/upgrade-authority.json
  export SOLANA_RPC=https://<your-private-rpc-endpoint>/<token>
  bash deploy-scripts/write-buffer-devnet.sh     # re-run this until it exits clean
  BUFFER_ACCOUNT_KEYPAIR=deploy-scripts/out/velocity-so-write-buffer-keypair.json \
    bash deploy-scripts/deploy-from-buffer-devnet.sh
  ```
  Every `write-buffer-devnet.sh` run reuses the buffer keypair file, and each pass closes more gaps
  until the buffer is whole.

- **Symptom: `Failed to parse ELF file: invalid section header` or
  `invalid account data for instruction`** when you run the swap with `program deploy --buffer …`.
  The buffer is **partial**. Some chunk-writes never landed, and nothing reported it, even though
  `write-buffer` exited 0. The CLI's exit code is not a reliable signal that the buffer is complete,
  because a last-batch retry that succeeds can hide earlier dropped writes. Fix it with:
  ```
  solana program show <BUFFER_PK> --url <rpc>     # compare Data Length to ls -l target/deploy/velocity.so
  bash deploy-scripts/write-buffer-devnet.sh      # re-run; the resume fills the missing chunks
  ```
  Two or three resume passes is normal. The buffer is whole when `solana program show` reports a
  Data Length close to the `.so` size. The BPF loader prepends a header of about 45 bytes. Adding
  `--with-compute-unit-price 1000` to the underlying `solana program write-buffer` invocation, or
  through the `write-buffer-devnet.sh` env, helps individual chunk-writes win contention faster.

- **Why the upload starts from scratch each time in the wrapper flow but not in the helper-script
  flow.** `anchor program upgrade` and `solana program deploy <file>` invent a new buffer pubkey per
  invocation and never expose it. On a partial failure the recent CLI tears the buffer down to
  refund the rent, so the next invocation has no buffer to resume into. The helper scripts keep the
  buffer keypair file on disk, so the next invocation finds the same partial buffer on chain and
  writes only the gaps.

- **Reclaim the SOL from an orphaned buffer.** Each abandoned buffer locks about 38 SOL, the devnet
  rent for a velocity-sized buffer:
  ```
  # List buffers under each candidate authority: the CLI default keypair and the upgrade authority
  solana program show --buffers --url <rpc>
  solana program show --buffers --url <rpc> \
    --buffer-authority $(solana-keygen pubkey "$VELOCITY_DEVNET_UPGRADE_KEYPAIR")

  # Close one
  solana program close <BUFFER_PK> --url <rpc> \
    --recipient $(solana-keygen pubkey "$VELOCITY_DEVNET_UPGRADE_KEYPAIR") \
    --buffer-authority "$VELOCITY_DEVNET_UPGRADE_KEYPAIR"

  # Close every buffer under one authority in one command
  solana program close --buffers --url <rpc> \
    --recipient $(solana-keygen pubkey "$VELOCITY_DEVNET_UPGRADE_KEYPAIR") \
    --buffer-authority "$VELOCITY_DEVNET_UPGRADE_KEYPAIR"
  ```
  `--buffer-authority` must point at the keypair file, which is the signer, not at the pubkey. When
  `--buffers` finds nothing under either candidate authority and the balances look intact, the CLI
  already closed the buffer and refunded the rent. Nothing more is needed.
- **The velocity keypair mismatch in `anchor build`.** The checked-in
  `target/deploy/velocity-keypair.json` is a placeholder, so `anchor build` fails with "Program ID
  mismatch" on a clean checkout. Pass `--ignore-keys`. The deployed program id is hardcoded in
  source, and the local keypair is unused for an upgrade.
- **Bun rejects a type re-exported without the `type` keyword.**
  `bun run deploy-scripts/init-devnet.ts` fails when the SDK re-exports a type that way, for example
  `export { PythLazerPriceFeedArray }`. Watch for it when you add a new SDK export.
- **A Pyth Lazer message must include `feedUpdateTimestamp`.** The on-chain
  `post_pyth_lazer_oracle_update` instruction skips an update whose payload lacks
  `FeedUpdateTimestamp`, and nothing reports it
  (`programs/velocity/src/instructions/pyth_lazer_oracle.rs`). The transaction returns Ok with no
  on-chain write, and the next phase fails with `Unable to read oracle price`. Phase C+ subscribes
  with `feedUpdateTimestamp`, with `bestBidPrice` and `bestAskPrice`, which give the confidence, and
  with `exponent`. Do not strip those properties.
- **The dUSDT oracle is a two-step init.** `handle_initialize_spot_market` requires the quote spot
  market to be `OracleSource::QuoteAsset` with `oracle = Pubkey::default()`. Phase E switches it to
  `PythLazerStableCoin` through `update_spot_market_oracle`, which reads the new oracle itself, so
  the USDT lazer PDA must already carry a posted price. That is why Phase C+ runs before Phase E and
  must succeed.
