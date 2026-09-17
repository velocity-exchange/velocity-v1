# @velocity-exchange/admin-cli

CLI for Velocity v1 admin operations. Sign with the right key, or pass a Squads
V4 multisig. The on-chain program enforces which tier of authority an action
requires.

## Install

```sh
npm install -g @velocity-exchange/admin-cli
# or run it once:
npx @velocity-exchange/admin-cli --help
```

## Usage

```sh
velocity-admin --help
```

## Profiles

A named profile bundles the connection settings so you do not repeat the
flags. Profiles are stored per user at `~/.config/velocity-admin/config.json`.
Set `VELOCITY_ADMIN_CONFIG` to use another path. Multisig addresses live only
in this local config and never in the repo.

```sh
velocity-admin config init            # interactive. Verifies everything against the live cluster
velocity-admin config list
velocity-admin config set-rpc <env> <url>   # shared per-cluster RPC, used by every profile without its own url
velocity-admin config set-default <name>
velocity-admin config remove <name>

velocity-admin -p mainnet-cold auth set-hot-admin accountExtension <pubkey>
VELOCITY_ADMIN_PROFILE=devnet velocity-admin extend-account --type state --dry-run
```

RPC URLs are shared per cluster under `rpcs` in the config. A profile normally
carries no url of its own and inherits the shared one for its env, so an RPC
key rotation is one `config set-rpc`. A profile can still pin its own url.

`config init` refuses to save anything it cannot verify. It classifies the RPC
by genesis hash and never by its URL. The keypair must load, and a multisig
must exist on that cluster. It matches the multisig's vault 0 against the live
State admins and reports a mismatch.

An explicit flag overrides the profile. Every command that reaches the chain
prints a one-line context header with the cluster, profile, signer, and
dispatch mode. A command exits when the declared env contradicts the genesis
hash the RPC reports. A mainnet direct send asks for interactive confirmation.
Pass `--yes` to skip it. `--yes` is implied when stdin is not a TTY.

## Commands

```
velocity-admin config init|list|set-rpc|set-default|remove   # connection profiles + shared RPCs (see Profiles above)
velocity-admin whoami                                # which on-chain authorities the signer holds

velocity-admin show config
velocity-admin show state   # every field of the State account, with the exchange-status, feature, LP-pool feature and solvency bitmasks decoded to bit names
velocity-admin show fees    # every fee users pay: trading tiers, filler reward, split, per-market adjustments + liquidation fees
velocity-admin show perp-markets [market]  # per-market risk + quoting params: OI cap, margins, spreads, jit/curve intensity, funding clamp, fee/pnl pool balances (the vAMM capital view)
velocity-admin show spot-markets [market]  # per-market lending params: deposit cap + headroom, weights, rate curve, withdraw guard, IF vault balance

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
velocity-admin perp-market set-spread-adjustment <market> <spreadAdjustment> <inventorySpreadAdjustment>  # VammQuoteManagement/warm/cold; both -100..100. Negative values need `--` first: set-spread-adjustment 0 -- -50 -25
velocity-admin perp-market set-funding-dead-zone <market> <threshold> <slope>
velocity-admin perp-market set-oracle-slot-delay <market> <slots>
velocity-admin perp-market deposit-fee-pool <market> <amount> [--source-vault <pk>]  # VaultDeposit hot key (or warm/cold); funds amm.fee_pool + total_fee_minus_distributions, raw quote base units
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
velocity-admin fees set-transaction-rails <inclusionLamports> <signatureLamports> <resourceFeeNumerator> <resourceFeeDenominator> <maxPriorityMicroLamportsPerCu>  # warm/cold admin; what a transaction costs to land. Every relay crank payment is derived from it, so this re-prices them all; the last argument caps the compute-unit price a liquidation crank reimburses (0 disables); markets take the new figures on their next set-market-clob
velocity-admin fees set-liquidation-crank-reimbursement <shareBps> <solSpotMarketIndex>  # cap on what the protocol repays a liquidation cranker (its priority fee, bounded by a share of the recovery), and the market pricing it in SOL; warm/cold admin
velocity-admin fees init-crank-treasury                                # warm/cold admin; create the one account every market's crank reservoir refills from. Run once, then fund it by sending SOL to the printed address
velocity-admin fees set-crank-treasury <refillTargetCranks> <refillWatermarkCranks>  # warm/cold admin; the two levels a market's crank reservoir is held between, counted in that market's dearest crank so one setting fits every market. The watermark is when a refill wakes and must cover the refill's own round trip; the target is how full it leaves the reservoir and must exceed it. A new watermark reaches a market on its next set-market-clob
velocity-admin fees withdraw-crank-treasury <lamports>                 # warm/cold admin; recover lamports from the crank treasury, never below rent
velocity-admin fees sweep-crank-reservoir <marketIndex> <lamports>     # warm/cold admin; move lamports from a market's reservoir back to the treasury (retired or over-provisioned markets)
velocity-admin fees set-taker-addon <market> <tenthBps>          # warm/cold admin; additive taker-fee add-on, -100..100 tenth-bps
velocity-admin fees set-promo-tier <tier>                        # warm/cold admin; promo fee-tier floor for everyone, 0 = off
velocity-admin fees set-referral-rate <percent>                  # warm/cold admin; Standard referrer reward on every active perp tier; Accelerated is a fixed constant
velocity-admin fees withdraw-perp <market> <amount>  # FeeWithdraw hot key; pays the recipient's ATA (created if needed)
velocity-admin fees withdraw-spot <market> <amount>  # FeeWithdraw hot key; pays the recipient's ATA (created if needed)
velocity-admin fees withdraw-protocol-user <market> <amount>  # FeeWithdraw hot key; drains settled crank rewards from the protocol-owned User (settle-pnl first)
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

velocity-admin quoter init <market> <quoterProgram> <user> <responseAccount> <quoteDisc> <executeDisc> [--type <vamm|clob|custom>] [--authority <pk>]  # born active but unapproved; custom entries must be created by the quoted user's authority; a clob entry designates the market's book, so run init-slab first; discs = 16 hex chars
velocity-admin quoter init-slab <market>                                   # permissionless; creates the market's QuoterSlabV0 with one slot (slot 0 = the book); approval right-sizes it from then on
velocity-admin quoter update-accounts <quoter> <metas...> --quote-indexes <csv> --execute-indexes <csv> [--authority <pk>]  # meta = "<pubkey>" or "<pubkey>:w"; replaces the whole list; the approved slab copy keeps serving until re-approved; a custom entry answers to its stored authority, a book's entry to the warm/cold admin
velocity-admin quoter update-config <quoter> [--response-account <pk>] [--quote-disc <hex>] [--execute-disc <hex>] [--authority <pk>]  # staged; the approved slab copy keeps serving until re-approved; a custom entry answers to its stored authority, a book's entry to the warm/cold admin
velocity-admin quoter set-active <quoter> <true|false> [--authority <pk>]  # maker kill switch; a custom entry's stored authority signs, a book's entry takes the warm/cold admin; written through to the slab slot
velocity-admin quoter set-approved <quoter> <true|false> [--admin <pk>]    # warm/cold admin vetting gate; copies the staging config into the market's slab slot (or revokes it); a book approval asks the book for its placement rules
velocity-admin quoter set-priority <quoter> <0-255> [--admin <pk>]         # warm/cold admin; lower fills first, pro rata within a tier; written through to the slab slot
velocity-admin quoter set-market-clob <market> <quoter> <clobMarket> [expireFallbackSlots] [--crank-cu <n>] [--crank-cu-<crank> <n>] [--admin <pk>]  # warm/cold admin; names the mandatory-baseline CLOB and stands up (or re-prices) the market's relay crank conditions + reservoir. Each crank's keeper payment is derived from the cost units it requests and the fee rails
velocity-admin quoter set-watch <quoter> --watch-account <pk> --offset <n> --len <n> [-a <pk>]  # custom entries only, their stored authority signs; declares the reprice region relay cross-discovery wakes on (len 0 clears); staged until re-approved
velocity-admin quoter set-oracle-band <quoter> <bps> [-a <pk>]             # entry authority; caps how far from oracle a Custom quoter's fills may price (0 clears); only ever tightens the market band, so it is written through to the slab slot
velocity-admin quoter attach-cross <quoter> [--fallback-slots <n>]           # permissionless; stands up (or re-prices) the entry's relay cross-discovery conditions

velocity-admin clob-market init <market> --clob-program <pk> [--capacity <n>] [--crank-cu <n>] [--crank-cu-<crank> <n>] [--relay-program <pk>|none] [book config flags]  # one-shot bring-up: book create+init, quoter slab when missing, quoter register+approve into the slab, canonical attach (creates crank conditions), relay watches (both blocks); warm/cold admin, direct-send only
velocity-admin clob-market register-watch <market> [--relay-program <pk>]  # register a relay WatchV0 over BOTH of an existing market's condition blocks (velocity's conditions account and the book's own); permissionless, direct-send only
velocity-admin clob-market update-config <market> [--tick-size <n>] [--step-size <n>] [--min-order-size <n>] [--blocking-min-size <n>] [--default-activation-delay <slots>] [--max-activation-delay <slots>] [--unknown-user-grace-slots <n>] [--evict-threshold <n>] [--max-quote-levels <n>] [--max-execute-fills <n>] [--max-execute-users <n>] [--reservation-grace-slots <n>]  # retune a live book through the CLOB's update_market_v0; only the flags passed are written. --reservation-grace-slots is how long a taker remainder's claim on the depth it crosses is honoured, so it is what bounds a cross crank that never lands (0..150). The book's authority signs

> **Turner scoping.** A market's conditions live on two accounts: velocity's
> crank-conditions PDA (the cross fallback poll) and the CLOB market itself
> (expiry, activation, a side at its eviction threshold, and a crossed book).
> Those are all facts about the book's own account, and the book keeps them
> current. Both accounts get a relay watch, and a turner must allow both
> programs:
> `--target-program <velocity-id>,<clob-id>`. Allowing only velocity filters
> the book's watches out at the registry query, and none of its cranks fire.


velocity-admin if stake <market> <amount> [--authority <pk>] [--user-token-account <pk>]  # inits the stake account if missing

velocity-admin wallet wrap-sol <lamports> [--authority <pk>] [--vault-index <i>] [--min-remaining <sol>] [--dry-run]  # wrap native SOL into the owner's wSOL ATA (created idempotently); one proposal with --multisig
velocity-admin wallet swap <inputMint> <outputMint> <amount> [--slippage-bps <bps>] [--only-direct-routes] [--vault-index <i>] [--dry-run]  # Jupiter swap from the owner wallet; with --multisig the route is quoted at proposal time, so approve and execute before it goes stale
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
velocity-admin multisig set-rent-collector <pubkey>              # propose a config tx setting the rent collector (required by close-accounts); executing any config tx marks still-Active vault proposals stale
velocity-admin multisig close-accounts [--dry-run]               # reclaim rent from settled proposals (Executed/Rejected/Cancelled + stale non-approved); requires the multisig's rent collector to be set

velocity-admin extend-account <account>                          # AccountExtension hot key (or warm/cold); grow one zero-copy account to the deployed program's size
velocity-admin extend-account --type <type> [--batch-size <n>] [--dry-run]  # migration crank: scan + extend every account of a type (see docs/ACCOUNT-EXTENSION.md)

velocity-admin call <ixName> <payloadFile>     # generic IDL escape hatch
velocity-admin batch <payloadFile> [--dry-run] # several instructions in ONE tx / vault proposal ({ instructions: [{ ix, args, accounts }, ...] }); one approval round, one timelock
```

## Common flows

Sequences that come up in treasury operations. Each step is a command above. Under a multisig
profile every step is a proposal that members approve and execute in the Squads UI.

**Fund a vault authority's trading account**: `wallet swap` to source the right token, then
`wallet wrap-sol` for SOL, then `user deposit`. A deposit fails while the market's hard cap has
no headroom. Check the headroom with `show spot-markets`, then raise the cap with
`spot-market set-max-token-deposits`. Raise `set-scale-initial-asset-weight-start` with it, or a
large depositor gets a derated collateral weight.

**Seed or top up an insurance fund**: `if stake <market> <amount>`. It initializes the stake
account on first use. Deposit caps do not apply.

**Fund perp pnl pools**: first `wallet transfer <quoteMint> <spotMarketVault> <amount>
--to-token-account`, an unattributed donation to the quote spot vault. After it lands, send a
`batch` of `updatePerpMarketPnlPool` instructions that attribute the amounts per market. The
order matters, because the update instruction checks that the vault holds the tokens.

**Fund vAMM fee pools**: `perp-market deposit-fee-pool <market> <amount>`, signed by the
VaultDeposit hot role. `show config` names the current holder. To fund several markets in one
proposal, send a `batch` of `depositIntoPerpMarketFeePool`. This is the cure when a market's
`total_fee_minus_distributions` has gone negative, because the deposit credits it one for one.
Run `perp-market sync-amm-summary-stats` first only when the accounting may have drifted from
the pools' real balances, since it recomputes rather than funds. Reserve resizing is a separate
operation. `recenterPerpMarketAmm` sets peg and sqrt_k in one warm or cold instruction, with
both values re-derived at the live oracle price. Reserves never adjust themselves to new capital.

**Execute a heavy proposal**: a CPI-heavy inner transaction such as a Jupiter swap exceeds the
Squads UI default of 200k compute units. Set about 1M in the UI execute modal, or run
`multisig execute` from a machine that holds a member key. An inner transaction never carries a
compute-budget instruction, because those cannot be called by CPI. A red UI simulation on an
unapproved proposal is therefore normal. Verify with `multisig inspect`.

**Listing a new market**: initialize and parameterise the market, then extend the market address
lookup table. `lut show` reports what is missing and `lut extend` adds it. Services build
versioned transactions against that table, so a fill or liquidation that touches a market
missing from it can exceed the transaction size limit. Extend the table before you redeploy the
services.

**Reclaim proposal rent**: run `multisig set-rent-collector`, then `multisig close-accounts`.
`set-rent-collector` is a config transaction, and it marks every still-Active vault proposal
stale, so choose the moment.

**Review a proposal before you approve it**: `multisig inspect <index>` is read-only and needs
no member key, so any reviewer can run it from their own machine. It answers the questions a
signer has, in plain terms:

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

The "what changes on chain" block simulates the proposal's own instructions with the vault as
the signer. It therefore produces a real before and after field diff at any proposal status.
Simulating the Squads execute wrapper instead would need the approvals first. Both snapshots
come from simulations issued back to back. An account that another program writes continuously,
such as a perp market's mm-oracle fields, therefore does not appear as a change the proposal
makes. When the chain moves between the two snapshots, the output says so. Add `--raw` for the
full argument list, the raw instruction data, and the complete logs. A non-velocity instruction
still shows its accounts, and a SOL transfer is decoded to an amount.

Every string that came off the chain goes through `ui.safe`, which replaces each control
character with U+FFFD. That covers program logs, decoded instruction arguments, decoded account
fields, and market names. Review simulates a proposal's inner instructions before approval, so a
proposer can put any program in a proposal and reach the reviewer's terminal with its `msg!`
output. An escape sequence in such a log could move the cursor and repaint the review with
forged output. Substitution rather than removal keeps the tampering visible.

## Output conventions

The read-heavy commands share the layout in `src/lib/ui.ts`: a `▌` section marker with that
section's verdict right-aligned, then `label   value` rows beneath it. Those commands are
`show`, `whoami`, `multisig proposals`, `multisig inspect`, and every dry run. Colour comes from
`picocolors`, which disables itself when stdout is not a TTY or `NO_COLOR` is set, so a redirect
to a file or a paste into Slack stays readable. Use these helpers rather than bare `console.log`
in a new command, so every command reads the same way.

## Routing through a Squads V4 multisig

Append `--multisig <multisigPda>` to any subcommand. When the multisig's vault 0
PDA must sign the action, for example because it holds the cold admin role, the
CLI submits one transaction that creates a `vault_transaction` and a `proposal`
against the multisig, with your wallet as the proposer. Members then approve and
execute the proposal in the Squads UI. When the vault does not need to sign, for
example because the wallet itself holds the required authority, a proposal would
change nothing. The CLI reports that and sends the transaction directly.

The user-scoped commands `user deposit`, `user withdraw`, `user set-delegate`,
and `if stake` default the authority to the multisig's vault 0 PDA under
`--multisig`, because the vault is what signs at execution. The vault must be
the velocity user or stake authority, and it must own the source token account.
`user deposit`, `user withdraw`, and `user set-delegate` also accept
`--vault-index` to target and propose against a vault other than 0.

`user init` follows the same pattern on mainnet. The program allows account
creation only when the authority signs or pays, so under `--multisig` the create
instructions go into one proposal and the vault PDA is the inner payer. The
vault must therefore hold enough SOL for the rent. Without `--multisig` the local
keypair is both authority and payer, and the CLI sends the transaction directly.

```sh
velocity-admin auth set-warm-admin <newWarmAdmin> \
  --multisig <multisigPda> \
  --keypair ~/cold-proposer.json
```

## Creating a Squads V4 multisig

`multisig create` creates a Squads V4 multisig. The current wallet becomes a 1/1
signer with Initiate, Vote, and Execute permissions. The proposer member can only
Initiate. The on-chain Squads program config supplies the treasury. An ephemeral
create-key seeds the multisig PDA.

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

The dispatcher derives no PDA. Supply every account.

### Batching

`batch` lands several instructions in one transaction, and so in one vault
proposal. That is one approval round and one timelock, so related admin
parameters can travel together.

```sh
velocity-admin batch <payloadFile.json> [--dry-run]
```

The payload is a list of `call`-shaped entries, each with the camelCase
instruction name under `ix`:

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

The rules match `call`. There is no PDA derivation, so supply every account.
Pass u64 arguments as JSON strings and pubkeys as base58 strings. Argument and
account names are camelCase, the form Anchor's TypeScript client exposes, rather
than the snake_case of the raw IDL file. Field names and types come from the
instruction's entry in `packages/sdk/src/idl/velocity.json`. Every instruction
lands in one inner transaction, so under `--multisig` the whole batch shares a
single proposal. `--dry-run` prints the built instructions and the expected
proposal rent first.

## Global options

| Flag                      | Default                                                                                   |
| ------------------------- | ----------------------------------------------------------------------------------------- |
| `-p, --profile <name>`    | `VELOCITY_ADMIN_PROFILE` env, else config default                                         |
| `-u, --url <url>`         | profile, else `https://api.mainnet-beta.solana.com`                                       |
| `-k, --keypair <path>`    | profile, else `~/.config/solana/id.json`                                                  |
| `-e, --env <env>`         | profile, else detected from the RPC's genesis hash                                        |
| `-m, --multisig <pubkey>` | profile, else none, which is a direct send. `--no-multisig` forces a direct send under a proposing profile |
| `-y, --yes`               | unset. A mainnet direct send asks for confirmation                                        |

## Authority introspection

```sh
velocity-admin whoami [-p <profile>]      # the State roles the signer holds, and multisig membership
velocity-admin multisig proposals [-m <pda>] [--limit <n>]  # recent proposals: status, approvals, timelock ETA
```

## Local development

Install once at the repo root, then work in this package:

```sh
bun install                       # at the repo root, for the whole workspace
cd packages/cli-admin
bun run start --help              # run from src through bun
bun run build                     # tsc into lib/
./lib/index.js --help             # run the compiled binary the way the published package does
```

The committed `package.json` depends on `"@velocity-exchange/sdk": "workspace:*"`,
so a local SDK edit takes effect at once. `.github/scripts/rewrite-workspace-deps.mjs`
rewrites that range to the concrete published version before a publish. Publishing
runs from CI on an `npm-cli-admin-v<version>` tag. See the repository CLAUDE.md for
the changeset and tag flow.
