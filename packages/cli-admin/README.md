# @velocity-exchange/admin-cli

CLI for Velocity v1 admin operations. Sign with the right key, or pass a Squads V4 multisig. The
on-chain program enforces which tier of authority each action requires.

## Install

```sh
npm install -g @velocity-exchange/admin-cli
# or, one-off:
npx @velocity-exchange/admin-cli --help
```

## Usage

```sh
velocity-admin --help
```

## Profiles

Connection settings can be bundled into named profiles instead of repeated flags. They are stored
per-user at `~/.config/velocity-admin/config.json`, which `VELOCITY_ADMIN_CONFIG` overrides.
Multisig addresses live only in this local config, deliberately not in the repo.

```sh
velocity-admin config init            # interactive; verifies everything against the live cluster
velocity-admin config list
velocity-admin config set-rpc <env> <url>   # shared per-cluster RPC, used by every profile without its own url
velocity-admin config set-default <name>
velocity-admin config remove <name>

velocity-admin -p mainnet-cold auth set-hot-admin accountExtension <pubkey>
VELOCITY_ADMIN_PROFILE=devnet velocity-admin extend-account --type state --dry-run
```

RPC URLs are shared per cluster through `rpcs` in the config. Profiles normally carry no url of
their own and inherit the shared one for their env, so rotating an RPC key is a single
`config set-rpc`. A profile can still pin its own url to deviate.

`config init` refuses to save anything it cannot verify. It classifies the RPC by genesis hash and
never by its URL, requires the keypair to load, and requires a multisig to exist on that cluster.
It matches that multisig's vault 0 against the live State admins and calls out any mismatch.

Explicit flags always override the profile. Every command that touches the chain prints a one-line
context header with the cluster, profile, signer and dispatch mode. A command dies when a declared
env contradicts the RPC's actual genesis hash. Mainnet direct sends ask for interactive
confirmation; pass `--yes` to skip, which is implied when stdin is not a TTY.

## Commands

```
velocity-admin config init|list|set-rpc|set-default|remove   # connection profiles + shared RPCs (see Profiles above)
velocity-admin whoami                                # which on-chain authorities the signer holds

velocity-admin show config
velocity-admin show state   # every field of the State account, with the exchange-status, feature, LP-pool feature and solvency bitmasks decoded to bit names
velocity-admin show fees    # every fee users pay: trading tiers, filler reward, split, per-market adjustments + liquidation fees
velocity-admin show perp-markets [market]  # per-market risk + quoting params: OI cap, margins, spreads + spread adjustments, jit/curve intensity, funding clamp, fee/pnl pool balances (the vAMM capital view)
velocity-admin show spot-markets [market]  # per-market lending params: deposit cap + headroom, weights, rate curve, withdraw guard, IF vault balance
velocity-admin show user [authority] [--vault-index n]  # UserStats plus each sub-account: delegate, status, collateral, health, leverage, spot balances, perp positions

velocity-admin audit withdrawals [market] [--hours <n>] [--min-usd <n>] [--limit <n>] [--history-pages <n>] [--json]  # read-only; the Spot Withdraw Breaker alert dump: breaker consumption per market at Grafana parity, then every withdrawal on the audited market attributed to the Velocity sub-account that made it (not the fee payer, which is usually our sponsor wallet), with each withdrawer's lifetime flows, PnL decomposition, live positions, 30d volume and account age. Facts only, no verdict. Defaults to the worst-consumed market

velocity-admin market payloads <symbol> [--out <dir>] [--spot-only|--perp-only] [--spot-params <file>] [--perp-params <file>]  # read-only; derive the listing payloads for a market from its entry in deploy-scripts/params, computing every PDA from the market index and feed id. Prints unless --out. Feed them to `propose-batch` in the printed order and read the ordering note: the oracle payload cannot share a batch with the market init
velocity-admin market fund <symbol> [--deposit <raw>] [--if-stake <raw>] [--fee-pool <raw>] [--pnl-pool <raw>] [--sub-account <n>]  # one Squads batch funding a listed market: lending deposit, IF stake, vAMM fee pool, pnl pool. All four sign as the same vault, so one proposal and one approval. Raw base units of the market each funds; omit a flag to skip that pool. Requires --multisig, the VaultDeposit hot role and tokens in the vault

velocity-admin auth set-cold-admin <pubkey>
velocity-admin auth set-warm-admin <pubkey>
velocity-admin auth set-pause-admin <pubkey>
velocity-admin auth set-hot-admin <role> <pubkey>
# Assign the vAMM active-management multisig (its Squads timelock is off-chain policy):
velocity-admin auth set-hot-admin vammQuoteManagement <multisig-pda>
velocity-admin auth init-config [--initial-warm <pk>]

velocity-admin perp-market set-status <market> <status>
velocity-admin perp-market set-fee-buffer <market> <amount>
velocity-admin perp-market set-bankruptcy-if-floor <market> <pct>  # PERCENTAGE_PRECISION (1e6); 0 selects the 10 bps default, "disabled" turns the floor off
velocity-admin perp-market set-spread-adjustment <markets> <spreadAdjustment> <inventorySpreadAdjustment>  # VammQuoteManagement/warm/cold; both -100..100. <markets> is 0, 0,1,4 or all, one tx. Negative values need `--` first: set-spread-adjustment all -- -50 -25
velocity-admin perp-market set-funding-dead-zone <market> <threshold> <slope>
velocity-admin perp-market set-oracle-slot-delay <market> <slots>
velocity-admin perp-market deposit-fee-pool <market> <amount> [--source-vault <pk>]  # VaultDeposit hot key (or warm/cold); funds amm.fee_pool + total_fee_minus_distributions, raw quote base units
velocity-admin perp-market deposit-pnl-pool <market> <amount> [--source-vault <pk>]  # VaultDeposit hot key (or warm/cold); transfers raw quote base units into the quote spot vault and credits perp_market.pnl_pool by the same amount in one ix. Prefer over `call updatePerpMarketPnlPool`, which credits without moving tokens
velocity-admin perp-market sync-amm-summary-stats <market> [--net-unsettled-funding-pnl <amount>]  # AmmCrank hot key (or warm/cold); recompute total_fee_minus_distributions from live state
velocity-admin spot-market set-status <market> <status>
velocity-admin spot-market set-guard-threshold <market> <threshold>
velocity-admin spot-market set-scale-initial-asset-weight-start <market> <start>  # warm/cold admin; QUOTE_PRECISION (1e6); 0 disables
velocity-admin spot-market set-fee-factors <market> <ifFeeFactor> <protocolFeeFactor>
velocity-admin spot-market set-withdraw-breaker <market> <pct>
velocity-admin spot-market set-max-token-deposits <market> <amount>  # warm/cold admin; hard deposit cap, raw token base units, 0 = uncapped
velocity-admin spot-market set-deposit-cap <market> <threshold> <pctPerDay>

velocity-admin exchange set-status <bitfield>
velocity-admin exchange sync-slot-duration <ms>        # permissionless; sync one IBRL transition (400->350->300->250->200) from its feature gate (previews the gate's activation/effective slots); effective slot derived onchain from the EpochSchedule
velocity-admin exchange set-solvency-status <bitfield>  # cold admin; gates solvency-repair ixs (1=solvencyRepairPaused)

velocity-admin feature-flags median-trigger-price <true|false>  # bit 2; enabling requires cold admin
velocity-admin feature-flags builder-codes <true|false>  # bit 4; enabling requires cold admin
velocity-admin feature-flags vamm-maker-rebate <true|false>  # bit 8; enabling requires cold admin

velocity-admin fees set-recipient <pubkey> <perp|spot>           # cold admin
velocity-admin fees set-schedule <t0bp> <t1bp> <t2bp> <t3bp> [--maker-rebate-bp <bp>] [--referrer <pct>] [--referee <pct>] [--amm-split <pct>] [--if-split <pct>] [--dry-run]  # warm/cold admin; rewrite the perp fee schedule in one ix (tier fees in bps; tiers 4-9 mirror tier 3; volume thresholds are program constants)
velocity-admin fees set-split <ammFeeNumerator> <ifFeeNumerator> # warm/cold admin
velocity-admin fees set-taker-addon <market> <tenthBps>          # warm/cold admin; additive taker-fee add-on, -100..100 tenth-bps
velocity-admin fees set-promo-tier <tier>                        # warm/cold admin; promo fee-tier floor for everyone, 0 = off
velocity-admin fees set-referral-rate <percent>                  # warm/cold admin; Standard referrer reward on every active perp tier; Accelerated is a fixed constant
velocity-admin fees withdraw-perp <market> <amount>  # FeeWithdraw hot key; pays the recipient's ATA (created if needed)
velocity-admin fees withdraw-spot <market> <amount>  # FeeWithdraw hot key; pays the recipient's ATA (created if needed)
velocity-admin fees sweep <market>                               # permissionless
velocity-admin fees settle-revenue-share <market> [escrowAuthority] [--all] # permissionless; pays accrued builder/referrer fees out of the pnl pool. --all settles every escrow still owed on the market, which delisting now requires
velocity-admin fees transfer-fee-pnl <feePoolMarket> <pnlPoolMarket> <amount> <fee-to-pnl|pnl-to-fee> # warm/cold admin

velocity-admin user init <name> [--sub-accounts <n>] [--authority <pk>] [--vault-index <i>] [--dry-run]  # authority must sign on mainnet; one proposal with --multisig, vault pays rent
velocity-admin user set-delegate <delegate> [--sub-accounts <n>] [--allow-transfer <bool>] [--authority <pk>] [--vault-index <i>] [--dry-run]  # authority signs; one proposal with --multisig
velocity-admin user set-special-status <user> <flags>
velocity-admin user set-equity-floor <user> <floor> <buffer>     # warm/cold admin; QUOTE_PRECISION raw units; floor 0 disables both checks
velocity-admin user reset-equity-breaker <userStats>             # warm/cold admin; unfreezes an authority after the breaker tripped
velocity-admin user set-accelerated-referral <authority> <accelerated>  # warm/cold admin; grant clears the auto-enrollment block, revoke sets it
velocity-admin user equity-floor-status <authority>              # read-only; per-subaccount equity/floor/buffer/headroom + level + breaker flag
velocity-admin user close-positions [--sub-accounts <csv>]       # signer = account authority; cancel all orders + close all perp positions reduce-only
velocity-admin user admin-deposit <market> <amount> --user <pk> --user-token-account <pk>
velocity-admin user deposit <market> <amount> [--authority <pk>] [--vault-index <i>] [--sub-account <id>] [--user-token-account <pk>] [--reduce-only] [--dry-run]
velocity-admin user withdraw <market> <amount> [--authority <pk>] [--vault-index <i>] [--sub-account <id>] [--user-token-account <pk>] [--reduce-only] [--dry-run]

velocity-admin if stake <market> <amount> [--authority <pk>] [--user-token-account <pk>]  # inits the stake account if missing

velocity-admin wallet wrap-sol <lamports> [--authority <pk>] [--vault-index <i>] [--min-remaining <sol>] [--dry-run]  # wrap native SOL into the owner's wSOL ATA (created idempotently); one proposal with --multisig
velocity-admin wallet swap <inputMint> <outputMint> <amount> [--slippage-bps <bps>] [--only-direct-routes] [--vault-index <i>] [--dry-run]  # Jupiter swap from the owner wallet; with --multisig the route is quoted at proposal time, so approve + execute promptly or it goes stale
velocity-admin wallet transfer <mint> <recipient> <amount> [--authority <pk>] [--vault-index <i>] [--to-token-account] [--dry-run]  # SPL transfer to the recipient's ATA (created idempotently); --to-token-account sends to a raw token account instead (e.g. a program vault donation); raw base units, checked against on-chain mint decimals; one proposal with --multisig
velocity-admin wallet balances [--authority <pk>] [--vault-index <i>]  # read-only: native SOL + token balances, velocity spot positions per sub-account, IF stakes

velocity-admin program upgrade --buffer <pk> [--spill <pk>] [--dry-run]  # propose an upgrade from an existing on-chain buffer
velocity-admin program halt [--so <path>]                        # deploy sbpf-asm-abort + propose an upgrade that bricks the program
velocity-admin program close-buffers [--dry-run] [--program-only|--metadata-only]  # reclaim rent from orphaned program + IDL buffers

velocity-admin lut show [address]                                # read-only; capacity, authority, and which live market accounts the table is missing
velocity-admin lut extend [address] [--dry-run]                  # add every missing market account; the set comes from State's market counts, so it cannot go stale. Signer must be the table authority

velocity-admin multisig create --proposer <pubkey> [--name <name>]  # create a Squads V4 1/1 multisig
velocity-admin multisig proposals [--limit <n>]                  # recent proposals: status, approvals, timelock ETA
velocity-admin multisig execute <index> [--cu-limit <units>] [--cu-price <microLamports>]  # execute an approved proposal as a member; sets a CU limit (Squads UI executes at the 200k default, too low for CPI-heavy inner txs)
velocity-admin multisig inspect <index> [--raw]                  # review a pending proposal: decoded instructions + args + named accounts, the account fields it would change (before -> after), program logs, and whether it can execute yet
velocity-admin multisig inspect-batch-tx <batchIndex> <n>  # decode inner transaction <n> (1-based) of a batch proposal: instructions, args, named accounts. A batch keeps its transactions in separate accounts, so `inspect` reports only the batch itself and `inspect <batchIndex>` says how many inner transactions it holds
velocity-admin multisig set-rent-collector <pubkey>              # propose a config tx setting the rent collector (required by close-accounts); executing any config tx marks still-Active vault proposals stale
velocity-admin multisig close-accounts [--dry-run]               # reclaim rent from settled proposals (Executed/Rejected/Cancelled + stale non-approved); requires the multisig's rent collector to be set

velocity-admin extend-account <account>                          # AccountExtension hot key (or warm/cold); grow one zero-copy account to the deployed program's size
velocity-admin extend-account --type <type> [--batch-size <n>] [--dry-run]  # migration crank: scan + extend every account of a type (see docs/ACCOUNT-EXTENSION.md)

velocity-admin call <ixName> <payloadFile>     # generic IDL escape hatch
velocity-admin batch <payloadFile> [--dry-run] # several instructions in ONE tx / vault proposal ({ instructions: [{ ix, args, accounts }, ...] }); one approval round, one timelock
velocity-admin propose-batch <payloadFiles...> [--dry-run]  # several payload files as ONE Squads batch: one proposal, one approval round, one timelock, N inner txs executed in order. Only each file has to fit 1232 bytes. Requires --multisig
```

## Common flows

These are the sequences that come up in treasury operations. Each step is a command from the list
above. Under a multisig profile, every step becomes a proposal that members approve and execute in
the Squads UI.

**Fund a vault authority's trading account**: `wallet swap` to source the right token, then
`wallet wrap-sol` if it is SOL, then `user deposit`. Deposits fail while the market's hard cap has
no headroom. Check with `show spot-markets` first and raise the cap with
`spot-market set-max-token-deposits`. Raise `set-scale-initial-asset-weight-start` alongside it, or
large depositors get their collateral weight derated.

**Seed or top up an insurance fund**: `if stake <market> <amount>`, which initializes the stake
account on first use. It is not subject to deposit caps.

**Fund perp pnl pools**: `perp-market deposit-pnl-pool <market> <amount>`, signed by the
VaultDeposit hot role. It transfers and credits in one instruction, so there is no window where the
vault holds tokens no balance claims. Fund several markets in one proposal with a `batch` of
`depositIntoPerpMarketPnlPool`. The older route, a `wallet transfer --to-token-account` donation
followed by a `batch` of `updatePerpMarketPnlPool`, still works and is still order-sensitive,
because the update instruction only credits and validates that the vault already holds the tokens.
Use it only against a program build that predates `deposit_into_perp_market_pnl_pool`.

**Fund vAMM fee pools (vAMM capital)**: `perp-market deposit-fee-pool <market> <amount>`, signed by
the VaultDeposit hot role, which `show config` lists. Fund several markets in one proposal with a
`batch` of `depositIntoPerpMarketFeePool`. This is the cure when a market's
`total_fee_minus_distributions` has gone negative, because the deposit credits it one for one.
Reach for `perp-market sync-amm-summary-stats` first only when you suspect the accounting has
drifted from the pools' real balances, since it recomputes rather than funds. Reserve resizing is a
separate operation: `recenterPerpMarketAmm` sets peg and sqrt_k in one instruction, takes warm or
cold, and wants values re-derived at the live oracle price. Reserves never adjust themselves to new
capital.

**List a new market**: `market payloads <symbol>` derives every payload from the reviewed entry in
`deploy-scripts/params` and computes the PDAs, so nothing is typed by hand. Propose the oracle
payload on its own first, because the lazer cranker has to post a price before the market init can
read it, then `propose-batch` the rest as one batch. Members read what they are approving with
`multisig inspect-batch-tx`. Once it executes, `market fund <symbol>` funds the lending pool, the
insurance fund, the vAMM fee pool and the pnl pool in a second proposal.

Then extend the market address lookup table. `lut show` reports what is missing and `lut extend`
adds it. Services build versioned transactions against that table, so a fill or liquidation
touching a market missing from it can exceed the transaction size limit. Extend the table before
you redeploy the services.

**Reclaim proposal rent**: `multisig set-rent-collector` first, then `multisig close-accounts`. The
first one is a config transaction, and it marks still-Active vault proposals stale, so time it.

**Execute heavy proposals**: anything CPI-heavy, such as a Jupiter swap, exceeds the Squads UI's
default 200k compute budget. Set roughly 1M in the UI's execute modal, or run `multisig execute`
from a machine holding a member key. Inner transactions never carry compute-budget instructions,
because those are not CPI-able, so a red UI simulation on an unapproved proposal is normal. Verify
with `multisig inspect`.

**Review a proposal before you approve it**: `multisig inspect <index>` is read-only and needs no
member key, so any reviewer can run it from their own machine. It answers the questions a signer
actually has, in plain terms:

```
▌ proposal #112                                              ! 3/4 approvals
   status        Active, 3 of 4 approvals
                 1 more approval needed to execute
   multisig      7qipzLR9j1JcvdxE1XJEFgvoyFmgBpgw5hMdHBMPcJtM
   proposer      prpHJmuXnqdaz92tBVdwsqmqyhqPLuq5Km35a5QWco3
   runs as       8jj7zJgdr5bDndc7evM74FMGwzLPmd4u4QxNzFi1BMai (vault 0)

▌ what it does                                                 1 instruction
   1. velocity updatePromoFeeTier
      promoFeeTier  3
      admin  signer  writable  8jj7zJgdr…  (this multisig's vault 0)
      state  ·       writable  2etx5NvPN…

▌ what changes on chain                          ✓ simulates clean, 2,464 CU
   State 2etx5NvPNxeMZ7EfHE6GjJfW2imRYEUANehNS1WB4CVW
      promoFeeTier  2 → 3

▌ program logs
   state.promo_fee_tier: 2 -> 3

▌ can it execute now                                                ! not yet
   pending approval
```

The "what changes" block is the point. It simulates the proposal's own instructions with the vault
as signer, so it produces a real before/after field diff at any proposal status. Simulating the
Squads execute wrapper instead would mean waiting for approval. Both snapshots come from
simulations issued back to back. That way accounts other programs write continuously, such as a
perp market's mm-oracle fields, do not show up as changes the proposal makes. If the chain moves
between the two snapshots, the output says so. Add `--raw` for the full argument list, the raw
instruction data and the complete logs. Non-velocity instructions still show their accounts, with
SOL transfers decoded to an amount.

Strings that came off the chain are rendered through `ui.safe`, which turns every control character
into U+FFFD. That covers program logs, decoded instruction arguments, decoded account fields and
market names. The reason is that a proposal's inner instructions are simulated during review,
before approval, so a proposer can put any program in a proposal and have its `msg!` output reach
the reviewer's terminal. Without `ui.safe`, an escape sequence in a log could move the cursor and
repaint the review with forged output. Substituting rather than dropping keeps the tampering
visible.

## Output conventions

The read-heavy commands share the layout in `src/lib/ui.ts`: a `▌` section marker with that
section's verdict right-aligned, then `label   value` rows beneath it. This covers `show`,
`whoami`, `multisig proposals`, `multisig inspect`, and every dry run. Colour comes from
`picocolors`, which turns itself off when stdout is not a TTY or when `NO_COLOR` is set, so
redirecting to a file or piping into Slack gives clean text. Prefer these helpers over bare
`console.log` in new commands, so the tool keeps reading as one thing.

`--agent` renders the same output for a program rather than a terminal: no colour, no alignment
padding, no truncated addresses, tables as tab-separated rows and labelled rows as `key=value`. It
is global, so it applies to any command. Commands that emit structured data offer `--json` as well,
which is the better target when you want the numbers rather than the layout.

## Routing through a Squads V4 multisig

Append `--multisig <multisigPda>` to any subcommand. If the multisig's vault 0 PDA is a required
signer of the action, for instance because it is the cold admin or the authority, the CLI submits a
single transaction that creates a `vault_transaction` and a `proposal` against the multisig with
your wallet as the proposer. Members then approve and execute through the Squads UI. If the vault
does not need to sign, for instance because the wallet itself is the required authority, a proposal
would be pointless. The CLI says so and sends the transaction directly instead.

User-scoped commands default the authority to the multisig's vault 0 PDA when `--multisig` is
passed, since the vault is what signs at execution. That covers `user deposit`, `user withdraw`,
`user set-delegate` and `if stake`. The vault must be the velocity user or stake authority, and
must own the source token account. `user deposit`, `user withdraw` and `user set-delegate` also
honor `--vault-index`, to target and propose against a vault other than 0.

`user init` follows the same pattern on mainnet. The program only allows account creation when the
authority signs or is the payer, so with `--multisig` the create instructions are batched into one
proposal and the vault PDA is the inner payer. The vault itself must hold enough SOL for the rent.
Without `--multisig` the local keypair is both authority and payer, and the transaction is sent
directly.

```sh
velocity-admin auth set-warm-admin <newWarmAdmin> \
  --multisig <multisigPda> \
  --keypair ~/cold-proposer.json
```

## Creating a Squads V4 multisig

`multisig create` provisions a fresh Squads V4 multisig with the current wallet as a 1/1 signer,
holding full Initiate, Vote and Execute permissions, plus a proposer member that can only Initiate
transactions. The on-chain Squads program config supplies the treasury, and an ephemeral create-key
seeds the multisig PDA.

```sh
velocity-admin multisig create \
  --proposer <proposerPubkey> \
  --name "Velocity Devnet Multisig" \
  --env devnet -u <devnetRpc>
```

## Generic dispatcher

For any velocity instruction without a dedicated wrapper:

```sh
velocity-admin call <camelCaseIxName> <payloadFile.json>
```

Example payload:

```json
{
	"args": { "withdrawGuardThreshold": "1000000000" },
	"accounts": {
		"spotMarket": "…",
		"state": "…",
		"adminAuthorityConfig": "…",
		"admin": "…"
	}
}
```

The dispatcher does no PDA derivation. Every account must be supplied.

### Batching

To land several instructions in ONE transaction or vault proposal, which means one approval round
and one timelock, use `batch`. Related admin params should ride together.

```sh
velocity-admin batch <payloadFile.json> [--dry-run]
```

The payload is a list of `call`-shaped entries, each with the camelCase instruction name under
`ix`:

```json
{
	"_comment": "optional notes: what this batch is, how values were derived",
	"instructions": [
		{
			"ix": "updateSpotMarketMaxTokenDeposits",
			"args": { "maxTokenDeposits": "5000000000000" },
			"accounts": {
				"admin": "<cold or warm vault PDA>",
				"state": "<state PDA (seed velocity_state)>",
				"spotMarket": "<spot market PDA (seeds spot_market + u16 LE index)>"
			}
		},
		{
			"ix": "updateSpotMarketScaleInitialAssetWeightStart",
			"args": { "scaleInitialAssetWeightStart": "5000000000000" },
			"accounts": { "…": "…" }
		}
	]
}
```

The rules match `call`: no PDA derivation, so supply every account, u64 args as JSON strings, and
pubkeys as base58 strings. Arg and account names are camelCase, as Anchor's TS client exposes them,
not the snake_case of the raw IDL file. Field names and types come from the instruction's entry in
`packages/sdk/src/idl/velocity.json`. All instructions land in one inner transaction, so with
`--multisig` the whole batch shares a single proposal. `--dry-run` prints the built instructions and
the expected proposal rent first.

### Batching past one transaction

`batch` puts everything in one inner transaction, so the whole payload has to fit Solana's
1232-byte limit. When it does not, `propose-batch` takes several payload files and proposes them as
one Squads batch:

```sh
velocity-admin propose-batch 1-oracle.json 2-init-spot.json 3-init-perp.json --dry-run
```

Each file becomes one inner transaction, in the order given, and only each file has to fit the
limit. There is still one proposal, one approval round and one timelock. Files use the same shape
as `batch`, `{ instructions: [...] }`. Requires `--multisig`.

Two things to know before using it. Execution is not atomic across inner transactions, so order
them to leave a coherent state wherever they stop. And members cannot read a batch's contents from
`multisig inspect`, which reports only the batch and how many inner transactions it holds, so point
them at `multisig inspect-batch-tx <batchIndex> <n>` to decode each one.

`--dry-run` simulates every inner transaction as the vault, in order, before proposing anything.
Groups run against the same chain state, so a group that depends on an earlier one's effect, a
market that does not exist yet for instance, reports an error there that is expected. The error
text is printed rather than judged, because only you know which of those are real.

## Global options

| Flag                      | Default                                                                                   |
| ------------------------- | ----------------------------------------------------------------------------------------- |
| `-p, --profile <name>`    | `VELOCITY_ADMIN_PROFILE` env, else config default                                         |
| `-u, --url <url>`         | profile, else `https://api.mainnet-beta.solana.com`                                       |
| `-k, --keypair <path>`    | profile, else `~/.config/solana/id.json`                                                  |
| `-e, --env <env>`         | profile, else detected from the RPC's genesis hash                                        |
| `-m, --multisig <pubkey>` | profile, else none: direct send (`--no-multisig` forces direct under a proposing profile) |
| `-y, --yes`               | (unset; mainnet direct sends ask for confirmation)                                        |
| `--agent`                 | (unset; renders for a terminal)                                                           |
| `--dry-run`               | (unset; builds, prices and prints without sending)                                        |

## Authority introspection

```sh
velocity-admin whoami [-p <profile>]      # which State roles the signer holds; multisig membership
velocity-admin multisig proposals [-m <pda>] [--limit <n>]  # recent proposals: status, approvals, timelock ETA
```

## Local development

Install once at the repo root, since this is a Bun workspace. Then work from
`packages/cli-admin/`:

```sh
bun install                    # once, at the repo root
cd packages/cli-admin
bun run start --help           # run from src directly via bun
bun run build                  # tsc into lib/
./lib/index.js --help          # run the compiled binary as the published package would
```

The committed `package.json` depends on `"@velocity-exchange/sdk": "workspace:*"`, so local edits
to the SDK are picked up immediately. `.github/scripts/rewrite-workspace-deps.mjs` rewrites that
range to a concrete version at publish time. Publishing itself goes through changesets and the
`npm-publish` workflow, driven by an `npm-cli-admin-v<version>` tag. See the Releases section of the
root [README](../../README.md).
